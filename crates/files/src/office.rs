//! Local OOXML paragraph translation for PowerPoint and Word.
pub(crate) mod amounts;
mod graphics;
mod inheritance;
mod layout;
mod review;
mod symbols;
mod word_fit;
pub use review::FrameReview;

use std::borrow::Cow;
use std::collections::{HashMap, HashSet};
use std::io::{Cursor, Read, Write};
use std::ops::Range;

use quick_xml::escape::escape;
use quick_xml::events::Event;
use quick_xml::name::ResolveResult;
use quick_xml::reader::{NsReader, Reader};
use thiserror::Error;
use zip::write::SimpleFileOptions;
use zip::{CompressionMethod, ZipArchive, ZipWriter};

const MAX_ARCHIVE: u64 = 128 * 1024 * 1024;
const MAX_TOTAL_UNCOMPRESSED: u64 = 512 * 1024 * 1024;
const MAX_XML: u64 = 8 * 1024 * 1024;
const MAX_ENTRIES: usize = 4096;
const DRAWING: &[u8] = b"http://schemas.openxmlformats.org/drawingml/2006/main";
const WORD: &[u8] = b"http://schemas.openxmlformats.org/wordprocessingml/2006/main";
const STRICT_WORD: &[u8] = b"http://purl.oclc.org/ooxml/wordprocessingml/main";
const STRICT_DRAWING: &[u8] = b"http://purl.oclc.org/ooxml/drawingml/main";

#[derive(Debug, Error)]
pub enum Error {
    #[error("Office package exceeds the size or entry limit")]
    Limit,
    #[error("unsupported Office package")]
    Package,
    #[error("unsupported or malformed Office XML")]
    Xml,
    #[error("Office translations do not match the extracted paragraphs or tags")]
    Translation,
    #[error(transparent)]
    Zip(#[from] zip::result::ZipError),
    #[error(transparent)]
    Io(#[from] std::io::Error),
}

#[derive(Clone, Copy, Debug)]
pub enum TagFormat {
    Anchor,
}

/// A model translation with validated anchors, or plain text to reinsert with the base run.
#[derive(Clone)]
pub enum OfficeTarget {
    Tagged(String),
    Simplified(String),
}

impl From<String> for OfficeTarget {
    fn from(text: String) -> Self {
        Self::Tagged(text)
    }
}

impl From<&str> for OfficeTarget {
    fn from(text: &str) -> Self {
        text.to_owned().into()
    }
}

pub const MAX_UNIT_TOKENS: usize = 1024;

#[derive(Clone, Copy, PartialEq, Eq)]
enum Format {
    PowerPoint,
    Word,
}

pub struct OfficeDocument {
    format: Format,
    for_translation: bool,
    bytes: Vec<u8>,
    parts: Vec<Part>,
    slide_numbers: HashMap<String, Vec<usize>>,
    pub unsupported: Vec<RetainedContent>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum RetentionReason {
    UnsupportedPart,
    ReadingGuide,
    Range,
    ProtectedStructure,
    InlineStructure,
    TableOfContents,
}

#[derive(Clone, Debug)]
pub struct RetainedContent {
    pub part: String,
    pub paragraph: Option<usize>,
    pub in_table: bool,
    pub count: usize,
    pub source: String,
    pub reason: RetentionReason,
}

impl std::fmt::Display for RetainedContent {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}", self.part)?;
        if let Some(paragraph) = self.paragraph {
            write!(
                f,
                ": paragraph {paragraph}{}",
                if self.in_table { " (table)" } else { "" }
            )?;
        }
        write!(
            f,
            ": retained {:?} ({}): {}",
            self.reason, self.count, self.source
        )
    }
}

pub struct Part {
    pub name: String,
    archive_index: usize,
    xml: String,
    pub paragraphs: Vec<Paragraph>,
    frames: Vec<layout::Frame>,
    word_regions: Vec<word_fit::Region>,
    symbol_theme: HashMap<String, String>,
    drawing_cache: Option<graphics::Cache>,
}

pub struct Paragraph {
    body: Range<usize>,
    font_size: Option<f64>,
    inherited_font_size: Option<f64>,
    inherited_ea: Option<String>,
    ordered_tags: bool,
    protected_run_formatting: bool,
    word_location: Option<(usize, bool)>,
    before: String,
    after: String,
    base: Run,
    tags: Vec<Tag>,
    embedded_atoms: Vec<EmbeddedAtom>,
    items: Vec<Item>,
}

struct EmbeddedAtom {
    tag: usize,
    source: Range<usize>,
    offset: usize,
}

#[derive(Clone)]
struct Run {
    write_before: String,
    write_after: String,
    key: String,
    props: Option<Range<usize>>,
    hyperlink: bool,
    word: bool,
}

enum Child {
    Run(Run, String),
    Atom(String),
    Embedded(String, Range<usize>, usize),
}

enum Tag {
    Style(Run),
    Atom(String),
}

enum Item {
    Text(String, Option<usize>),
    Atom(usize),
}

impl OfficeDocument {
    pub fn read(path: &std::path::Path) -> Result<Self, Error> {
        Self::read_mode(path, false, Self::format_for_path(path))
    }

    pub fn read_for_translation(path: &std::path::Path) -> Result<Self, Error> {
        Self::read_mode(path, true, Self::format_for_path(path))
    }

    fn format_for_path(path: &std::path::Path) -> Format {
        if path
            .extension()
            .is_some_and(|ext| ext.eq_ignore_ascii_case("docx"))
        {
            Format::Word
        } else {
            Format::PowerPoint
        }
    }

    pub fn extension(&self) -> &'static str {
        match self.format {
            Format::PowerPoint => "pptx",
            Format::Word => "docx",
        }
    }

    fn read_mode(
        path: &std::path::Path,
        for_translation: bool,
        format: Format,
    ) -> Result<Self, Error> {
        let mut bytes = Vec::new();
        std::fs::File::open(path)?
            .take(MAX_ARCHIVE + 1)
            .read_to_end(&mut bytes)?;
        Self::open_format(bytes, for_translation, format)
    }

    pub fn open(bytes: Vec<u8>) -> Result<Self, Error> {
        Self::open_mode(bytes, false)
    }

    fn open_mode(bytes: Vec<u8>, for_translation: bool) -> Result<Self, Error> {
        Self::open_format(bytes, for_translation, Format::PowerPoint)
    }

    fn open_format(bytes: Vec<u8>, for_translation: bool, format: Format) -> Result<Self, Error> {
        if bytes.len() as u64 > MAX_ARCHIVE {
            return Err(Error::Limit);
        }
        let mut archive = ZipArchive::new(Cursor::new(&bytes))?;
        if archive.len() > MAX_ENTRIES {
            return Err(Error::Limit);
        }
        if archive.index_for_name("[Content_Types].xml").is_none()
            || archive
                .index_for_name(match format {
                    Format::PowerPoint => "ppt/presentation.xml",
                    Format::Word => "word/document.xml",
                })
                .is_none()
        {
            return Err(Error::Package);
        }
        let mut parts = Vec::new();
        let mut unsupported = Vec::new();
        let mut total = 0u64;
        let mut names = HashSet::new();
        for index in 0..archive.len() {
            let raw = archive.by_index_raw(index)?;
            let name = raw.name()?.into_owned();
            total = total.saturating_add(raw.size());
            if total > MAX_TOTAL_UNCOMPRESSED {
                return Err(Error::Limit);
            }
            if raw.encrypted()
                || raw.is_symlink()
                || raw.enclosed_name().is_none()
                || !names.insert(name.clone())
            {
                return Err(Error::Package);
            }
            let graphic = (format == Format::PowerPoint)
                .then(|| graphics::Graphic::of(&name))
                .flatten();
            let (editable, report_untouched) = match format {
                Format::PowerPoint => (
                    slide_or_notes(&name) || graphic.is_some(),
                    for_translation && unsupported_text_part(&name),
                ),
                Format::Word => (word_body_part(&name), name == "word/comments.xml"),
            };
            if !editable && !report_untouched {
                continue;
            }
            if raw.size() > MAX_XML {
                return Err(Error::Limit);
            }
            drop(raw);
            let mut xml = String::new();
            archive
                .by_index(index)?
                .take(MAX_XML + 1)
                .read_to_string(&mut xml)?;
            if xml.len() as u64 > MAX_XML {
                return Err(Error::Limit);
            }
            if !editable {
                if has_untranslated_text(&xml)? {
                    unsupported.push(RetainedContent {
                        part: name.clone(),
                        paragraph: None,
                        in_table: false,
                        count: 1,
                        source: String::new(),
                        reason: RetentionReason::UnsupportedPart,
                    });
                }
                continue;
            }
            let (mut paragraphs, skipped) =
                parse_paragraph_details(&xml, for_translation && graphic.is_none(), format)?;
            let mut drawing_cache = None;
            if let Some(graphic) = graphic {
                let (scopes, cache_id) = graphic.scopes(&xml)?;
                paragraphs.retain(|p| {
                    scopes
                        .iter()
                        .any(|range| p.body.start >= range.start && p.body.end <= range.end)
                });
                if matches!(graphic, graphics::Graphic::Diagram) {
                    match graphics::diagram_cache(
                        &mut archive,
                        &name,
                        cache_id.as_deref(),
                        &paragraphs,
                    )? {
                        Some(cache) => drawing_cache = cache,
                        None => paragraphs.clear(),
                    }
                }
                if for_translation && has_text_outside(&xml, paragraphs.iter().map(|p| &p.body))? {
                    unsupported.push(RetainedContent {
                        part: name.clone(),
                        paragraph: None,
                        in_table: false,
                        count: 1,
                        source: String::new(),
                        reason: RetentionReason::UnsupportedPart,
                    });
                }
            }
            let mut frames = if for_translation && format == Format::PowerPoint && graphic.is_none()
            {
                layout::frames(&xml)?
            } else {
                Vec::new()
            };
            // Resolve presentation defaults after selecting reading guides, so
            // additional layout information cannot change translation units.
            let original_guides: HashSet<_> = paragraphs
                .iter()
                .filter(|p| {
                    let i = frames.partition_point(|f| f.range.end <= p.body.start);
                    frames.get(i).is_some_and(|f| {
                        !f.is_table_cell() && f.contains(p) && f.likely_furigana(p)
                    })
                })
                .map(|p| p.body.start)
                .collect();
            let symbol_theme = if !frames.is_empty() {
                inheritance::resolve_frames(
                    &mut archive,
                    &name,
                    &xml,
                    &mut frames,
                    &mut paragraphs,
                )?
            } else {
                HashMap::new()
            };
            layout::mark_overlaps(&mut frames);
            let mut furigana = 0;
            paragraphs.retain(|p| {
                let index = frames.partition_point(|frame| frame.range.end <= p.body.start);
                let retain = original_guides.contains(&p.body.start)
                    || frames.get(index).is_some_and(|frame| {
                        frame.is_table_cell() && frame.contains(p) && frame.likely_furigana(p)
                    });
                furigana += usize::from(retain);
                !retain
            });
            if furigana > 0 {
                unsupported.push(RetainedContent {
                    part: name.clone(),
                    paragraph: None,
                    in_table: false,
                    count: furigana,
                    source: String::new(),
                    reason: RetentionReason::ReadingGuide,
                });
            }
            unsupported.extend(
                skipped
                    .iter()
                    .filter(|item| {
                        item.text.chars().any(char::is_alphabetic)
                            || item.reason == RetentionReason::TableOfContents
                    })
                    .map(|item| RetainedContent {
                        part: name.clone(),
                        paragraph: Some(item.index),
                        in_table: item.in_table,
                        count: 1,
                        source: item.text.clone(),
                        reason: item.reason,
                    }),
            );
            let word_regions = if for_translation && format == Format::Word {
                word_fit::regions(&xml)?
            } else {
                Vec::new()
            };
            parts.push(Part {
                symbol_theme,
                word_regions,
                name,
                archive_index: index,
                xml,
                paragraphs,
                frames,
                drawing_cache,
            });
        }
        // A linked drawing cache is rewritten from the data part's targets.
        // Standalone or unrecognized drawings still have a retention report.
        unsupported.retain(|item| {
            !parts.iter().any(|part| {
                part.drawing_cache
                    .as_ref()
                    .is_some_and(|cache| cache.name == item.part)
            })
        });
        if parts.is_empty() {
            return Err(Error::Package);
        }
        let slide_numbers = if format == Format::PowerPoint {
            review::slide_numbers(&mut archive)?
        } else {
            HashMap::new()
        };
        Ok(Self {
            format,
            for_translation,
            bytes,
            parts,
            slide_numbers,
            unsupported,
        })
    }

    pub fn parts(&self) -> &[Part] {
        &self.parts
    }

    /// Fits needing user review, including successful light shrinking. The same
    /// fit decisions are used by write(); copied text has no translation tags.
    pub fn powerpoint_layout_reviews(
        &self,
        format: TagFormat,
        targets: &[Vec<Option<OfficeTarget>>],
    ) -> Vec<FrameReview> {
        let mut reviews = Vec::new();
        if self.format != Format::PowerPoint {
            return reviews;
        }
        for (part, targets) in self.parts.iter().zip(targets) {
            if part.name.starts_with("ppt/notesSlides/") {
                continue;
            }
            for (frame, indices, fit) in part.fits(format, targets) {
                if !fit.overflow && !fit.applies() {
                    continue;
                }
                let paragraphs = &part.paragraphs[indices.clone()];
                let source = paragraphs
                    .iter()
                    .map(Paragraph::plain_text)
                    .collect::<Vec<_>>()
                    .join(" ");
                let translated = paragraphs
                    .iter()
                    .zip(targets.iter().skip(indices.start))
                    .filter_map(|(p, t)| {
                        t.as_ref()
                            .and_then(|t| p.visible_office_target(format, t).ok())
                    })
                    .collect::<Vec<_>>()
                    .join("\n");
                reviews.push(FrameReview {
                    part: part.name.clone(),
                    slide: self
                        .slide_numbers
                        .get(&part.name)
                        .and_then(|s| s.first().copied()),
                    paragraph: false,
                    notes: part.name.starts_with("ppt/notesSlides/"),
                    frame: frame.index,
                    source,
                    translated,
                    scale_percent: (fit.scale * 100.0).round() as u8,
                    overflow: fit.overflow,
                });
            }
            // Merged cells, chart titles and SmartArt labels can have their
            // final bounds assigned by the viewer. Report growth without editing geometry.
            for (i, (p, t)) in part.paragraphs.iter().zip(targets).enumerate() {
                let index = part.frames.partition_point(|f| f.range.end <= p.body.start);
                if part.frames.get(index).is_some_and(|f| f.contains(p)) {
                    continue;
                }
                let Some(translated) = t
                    .as_ref()
                    .and_then(|t| p.visible_office_target(format, t).ok())
                else {
                    continue;
                };
                let source = p.plain_text();
                if layout::text_width(&translated) <= layout::text_width(&source)
                    && translated.lines().count() <= source.lines().count()
                {
                    continue;
                }
                let row = FrameReview {
                    part: part.name.clone(),
                    slide: None,
                    notes: part.name.starts_with("ppt/notesSlides/"),
                    frame: i + 1,
                    paragraph: true,
                    source,
                    translated,
                    scale_percent: 100,
                    overflow: true,
                };
                if let Some(slides) = self.slide_numbers.get(&part.name) {
                    reviews.extend(slides.iter().map(|&slide| FrameReview {
                        slide: Some(slide),
                        ..row.clone()
                    }));
                } else {
                    reviews.push(row);
                }
            }
        }
        reviews
    }

    /// Outer paragraph numbers in each Word part, independent of viewer pagination.
    pub fn word_overflow_reviews(
        &self,
        format: TagFormat,
        targets: &[Vec<Option<OfficeTarget>>],
    ) -> Vec<(String, usize)> {
        let mut locations = Vec::new();
        for (part, targets) in self.parts.iter().zip(targets) {
            for r in &part.word_regions {
                if let Some(d) = r.decide(&part.paragraphs, targets, format) {
                    locations.push((part.name.clone(), d.paragraph));
                }
            }
        }
        locations.sort_unstable();
        locations.dedup();
        locations
    }

    /// The original package is returned byte-for-byte when no paragraph changes.
    pub fn write(
        &self,
        format: TagFormat,
        target_language: &str,
        translations: &[Vec<Option<OfficeTarget>>],
    ) -> Result<Vec<u8>, Error> {
        if translations.len() != self.parts.len() {
            return Err(Error::Translation);
        }
        let mut changed = std::collections::HashMap::new();
        for (part, targets) in self.parts.iter().zip(translations) {
            if targets.len() != part.paragraphs.len() {
                return Err(Error::Translation);
            }
            let mut edits: Vec<_> = part
                .word_regions
                .iter()
                .filter_map(|r| r.decide(&part.paragraphs, targets, format))
                .flat_map(|decision| decision.edits)
                .collect();
            let mut fitted = vec![None; part.paragraphs.len()];
            for (frame, indices, fit) in part.fits(format, targets) {
                if fit.wrap
                    && let Some(range) = frame.wrap_edit()
                {
                    let body = &part.xml[range.clone()];
                    let end = {
                        let mut reader = quick_xml::Reader::from_str(body);
                        reader.read_event().map_err(|_| Error::Xml)?;
                        reader.buffer_position() as usize
                    };
                    let opening = layout::rewrite_tag(&body[..end], &[(b"wrap", "square")])?;
                    edits.push((range.start..range.start + end, opening));
                }
                if fit.applies() {
                    fitted[indices].fill(Some(fit));
                }
            }
            // Descendants must be rendered first, then inserted into the parent's
            // opaque drawing atom. Applying overlapping XML edits loses either
            // the inner translation or the surrounding paragraph.
            for ((p, target), fit) in part.paragraphs.iter().zip(targets).zip(fitted).rev() {
                if let Some(target) = target {
                    if self.for_translation
                        && !p.target_amounts_match(format, target, target_language)
                    {
                        return Err(Error::Translation);
                    }
                    let mut rendered = p.rebuild_target(format, target, target_language, &edits)?;
                    if let Some(fit) = fit {
                        rendered =
                            fit.compact_paragraph_with_default(rendered, p.inherited_font_size)?;
                    }
                    edits.retain(|(range, _)| {
                        !(p.body.start <= range.start && range.end <= p.body.end)
                    });
                    edits.push((p.body.clone(), rendered));
                }
            }
            if let Some(cache) = &part.drawing_cache {
                let mut cache_edits = Vec::new();
                for (index, p) in &cache.links {
                    if let Some(target) = &targets[*index] {
                        cache_edits.push((
                            p.body.clone(),
                            p.rebuild_target(format, target, target_language, &[])?,
                        ));
                    }
                }
                if !cache_edits.is_empty() {
                    let xml = symbols::apply(
                        &cache.xml,
                        cache_edits,
                        cache.links.iter().map(|(i, p)| (p, targets[*i].is_some())),
                        &part.symbol_theme,
                        true,
                    )?;
                    paragraphs(&xml)?;
                    if changed.insert(cache.index, xml).is_some() {
                        return Err(Error::Translation);
                    }
                }
            }
            if !edits.is_empty() {
                let xml = symbols::apply(
                    &part.xml,
                    edits,
                    part.paragraphs
                        .iter()
                        .zip(targets)
                        .map(|(p, t)| (p, t.is_some())),
                    &part.symbol_theme,
                    self.format == Format::PowerPoint,
                )?;
                // Reject malformed XML before copying it into the package.
                if self.format == Format::PowerPoint {
                    paragraphs(&xml)?;
                } else {
                    parse_office_paragraphs(&xml, false, self.format)?;
                }
                changed.insert(part.archive_index, xml);
            }
        }
        if changed.is_empty() {
            return Ok(self.bytes.clone());
        }
        let mut archive = ZipArchive::new(Cursor::new(&self.bytes))?;
        let mut output = ZipWriter::new(Cursor::new(Vec::new()));
        for index in 0..archive.len() {
            let entry = archive.by_index_raw(index)?;
            if let Some(xml) = changed.remove(&index) {
                output.start_file(
                    entry.name()?,
                    SimpleFileOptions::default().compression_method(CompressionMethod::Deflated),
                )?;
                output.write_all(xml.as_bytes())?;
            } else {
                output.raw_copy_file(entry)?;
            }
        }
        Ok(output.finish()?.into_inner())
    }
}

impl Part {
    pub fn source_context(&self) -> String {
        if !self.name.starts_with("ppt/") {
            return String::new();
        }
        let mut unique = HashSet::new();
        self.paragraphs
            .iter()
            .map(Paragraph::plain_text)
            .filter(|text| unique.insert(text.clone()))
            .collect::<Vec<_>>()
            .join("\n")
    }

    /// Linked caches have their own paragraph XML. Keep strict tags for both copies.
    pub fn anchor_fallback_source(&self, index: usize) -> Option<String> {
        if self.drawing_cache.is_some() {
            return None;
        }
        self.paragraphs[index].anchor_fallback_source()
    }

    fn fits<'a>(
        &'a self,
        format: TagFormat,
        results: &'a [Option<OfficeTarget>],
    ) -> impl Iterator<Item = (&'a layout::Frame, Range<usize>, layout::Fit)> {
        let mut fits: Vec<_> = self
            .frames
            .iter()
            .filter_map(|frame| {
                let start = self
                    .paragraphs
                    .partition_point(|p| p.body.start < frame.range.start);
                let end = self
                    .paragraphs
                    .partition_point(|p| p.body.start < frame.range.end);
                let paragraphs = self.paragraphs[start..end]
                    .iter()
                    .zip(results.iter().skip(start).map(Option::as_ref));
                frame
                    .fit(paragraphs, format)
                    .map(|fit| (frame, start..end, fit))
            })
            .collect();
        // Align only boxes which already require shrinking and share a text
        // role, level and starting size. Keep fitting boxes at their own size.
        let mut scales = HashMap::new();
        let group = |frame: &layout::Frame, indices: &Range<usize>| {
            if frame.grouped {
                return None;
            }
            let ps = &self.paragraphs[indices.clone()];
            if !ps
                .iter()
                .zip(results.iter().skip(indices.start))
                .any(|(p, t)| {
                    t.as_ref()
                        .and_then(|t| p.visible_office_target(format, t).ok())
                        .is_some_and(|t| t != p.plain_text())
                })
            {
                return None;
            }
            let first = ps.first()?;
            let level = inheritance::paragraph_level(first);
            let size = first.font_size?;
            ps.iter()
                .all(|p| p.font_size == Some(size) && inheritance::paragraph_level(p) == level)
                .then_some((frame.title, frame.is_table_cell(), level, size.to_bits()))
        };
        for (frame, indices, fit) in &fits {
            if fit.scale < 1.0
                && let Some(k) = group(frame, indices)
            {
                scales
                    .entry(k)
                    .and_modify(|s: &mut f64| *s = s.min(fit.scale))
                    .or_insert(fit.scale);
            }
        }
        for (frame, indices, fit) in &mut fits {
            if fit.scale < 1.0
                && let Some(k) = group(frame, indices)
            {
                fit.scale = scales[&k];
            }
        }
        fits.into_iter()
    }

    /// Locations requiring a native Word layout check. No geometry is rewritten.
    /// Count outer paragraphs, matching word_location(), not textbox fallback copies.
    pub fn word_layout_risks(&self) -> Vec<usize> {
        if !self.name.starts_with("word/") {
            return Vec::new();
        }
        let mut reader = NsReader::from_str(&self.xml);
        let mut paragraphs = 0;
        let mut depth: usize = 0;
        let mut risks = Vec::new();
        while let Ok((ns, event)) = reader.read_resolved_event() {
            let word = matches!(&ns, ResolveResult::Bound(n) if n.as_ref() == WORD || n.as_ref() == STRICT_WORD);
            match event {
                Event::Start(ref tag) | Event::Empty(ref tag) => {
                    let name = tag.local_name();
                    if word && name.as_ref() == b"p" {
                        if depth == 0 {
                            paragraphs += 1;
                        }
                        if matches!(event, Event::Start(_)) {
                            depth += 1;
                        }
                    }
                    let floating = name.as_ref() == b"anchor"
                        && matches!(&ns, ResolveResult::Bound(n) if n.as_ref().ends_with(b"/wordprocessingDrawing"));
                    let vml = name.as_ref() == b"shape"
                        && matches!(&ns, ResolveResult::Bound(n) if n.as_ref() == b"urn:schemas-microsoft-com:vml")
                        && tag.attributes().flatten().any(|a| {
                            a.key.as_ref() == b"style"
                                && a.value.windows(17).any(|w| w == b"position:absolute")
                        });
                    if depth > 0 && (floating || vml) {
                        risks.push(paragraphs);
                    }
                    if word && name.as_ref() == b"tblpPr" {
                        risks.push(paragraphs + 1);
                    }
                    if word
                        && name.as_ref() == b"trHeight"
                        && tag.attributes().flatten().any(|a| {
                            a.key.local_name().as_ref() == b"hRule" && a.value.as_ref() == b"exact"
                        })
                    {
                        risks.push(paragraphs + 1);
                    }
                }
                Event::End(tag) if word && tag.local_name().as_ref() == b"p" => {
                    depth = depth.saturating_sub(1);
                }
                Event::Eof => break,
                _ => {}
            }
        }
        risks.sort_unstable();
        risks.dedup();
        risks
    }

    pub fn overflow_frames(
        &self,
        format: TagFormat,
        results: &[Option<OfficeTarget>],
    ) -> Vec<usize> {
        self.fits(format, results)
            .filter_map(|(frame, _, fit)| fit.overflow.then_some(frame.index))
            .collect()
    }
}

fn unsupported_text_part(name: &str) -> bool {
    name.ends_with(".xml")
        && [
            "ppt/charts/",
            "ppt/slideLayouts/",
            "ppt/slideMasters/",
            "ppt/notesMasters/",
            "ppt/diagrams/",
        ]
        .iter()
        .any(|prefix| name.starts_with(prefix))
}

fn has_untranslated_text(xml: &str) -> Result<bool, Error> {
    has_text_outside(xml, std::iter::empty())
}

fn has_text_outside<'a>(
    xml: &str,
    covered: impl Iterator<Item = &'a Range<usize>>,
) -> Result<bool, Error> {
    let mut covered = covered.peekable();
    let mut reader = quick_xml::Reader::from_str(xml);
    let mut in_text = false;
    loop {
        let at = reader.buffer_position() as usize;
        while covered.peek().is_some_and(|range| range.end <= at) {
            covered.next();
        }
        let outside = !covered.peek().is_some_and(|range| range.contains(&at));
        match reader.read_event().map_err(|_| Error::Xml)? {
            Event::Start(node) => in_text = matches!(node.local_name().as_ref(), b"t" | b"v"),
            Event::End(_) => in_text = false,
            Event::Text(text) if in_text && outside => {
                if text
                    .decode()
                    .map_err(|_| Error::Xml)?
                    .chars()
                    .any(char::is_alphabetic)
                {
                    return Ok(true);
                }
            }
            Event::CData(text) if in_text && outside => {
                if text
                    .decode()
                    .map_err(|_| Error::Xml)?
                    .chars()
                    .any(char::is_alphabetic)
                {
                    return Ok(true);
                }
            }
            Event::DocType(_) => return Err(Error::Xml),
            Event::Eof => return Ok(false),
            _ => {}
        }
    }
}

fn slide_or_notes(name: &str) -> bool {
    ["ppt/slides/slide", "ppt/notesSlides/notesSlide"]
        .iter()
        .any(|prefix| {
            name.strip_prefix(prefix)
                .and_then(|tail| tail.strip_suffix(".xml"))
                .is_some_and(|n| !n.is_empty() && n.bytes().all(|b| b.is_ascii_digit()))
        })
}

fn word_body_part(name: &str) -> bool {
    matches!(
        name,
        "word/document.xml" | "word/footnotes.xml" | "word/endnotes.xml"
    ) || ["word/header", "word/footer"].into_iter().any(|prefix| {
        name.strip_prefix(prefix)
            .and_then(|stem| stem.strip_suffix(".xml"))
            .is_some_and(|index| !index.is_empty() && index.bytes().all(|b| b.is_ascii_digit()))
    })
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum NodeKind {
    Paragraph,
    Properties,
    EndProperties,
    Run,
    RunProperties,
    ProtectedRunFormatting,
    DefaultRunProperties,
    Text,
    Break,
    Field,
    Link,
    WordLink,
    WordRange,
    WordRangeStart,
    WordRangeEnd,
    WordPrefix,
    WordSuffix,
    WordFieldChar,
    WordFieldText,
    WordDrawing,
    WordTextBox,
    WordFallback,
    WordControl,
    WordControlContent,
    WordProtected,
    WordBoundary,
    WordDeletedText,
    WordScopedRun,
    Proofing,
    Bullet,
    SpacingAfter,
    LineSpacing,
    Other,
}

#[derive(Clone)]
struct Node {
    kind: NodeKind,
    full: Range<usize>,
    body: Range<usize>,
    children: Vec<Node>,
    text: String,
    in_word_table: bool,
    word_paragraph: bool,
}

fn kind(namespace: ResolveResult<'_>, name: &[u8], format: Format) -> NodeKind {
    if format == Format::Word
        && name == b"Fallback"
        && matches!(&namespace, ResolveResult::Bound(ns) if ns.as_ref() == b"http://schemas.openxmlformats.org/markup-compatibility/2006")
    {
        return NodeKind::WordFallback;
    }
    if format == Format::Word
        && name == b"AlternateContent"
        && matches!(&namespace, ResolveResult::Bound(ns) if ns.as_ref() == b"http://schemas.openxmlformats.org/markup-compatibility/2006")
    {
        return NodeKind::WordDrawing;
    }
    if format == Format::Word
        && name == b"t"
        && matches!(&namespace, ResolveResult::Bound(ns) if [DRAWING, STRICT_DRAWING].contains(&ns.as_ref()))
    {
        return NodeKind::Text;
    }
    let namespaces = match format {
        Format::PowerPoint => [DRAWING, STRICT_DRAWING],
        Format::Word => [WORD, STRICT_WORD],
    };
    if !matches!(namespace, ResolveResult::Bound(ns) if namespaces.contains(&ns.as_ref())) {
        return NodeKind::Other;
    }
    match name {
        b"drawing" | b"pict" if format == Format::Word => NodeKind::WordDrawing,
        b"txbxContent" if format == Format::Word => NodeKind::WordTextBox,
        b"sdt" if format == Format::Word => NodeKind::WordControl,
        b"sdtContent" if format == Format::Word => NodeKind::WordControlContent,
        b"ins" | b"del" | b"moveFrom" | b"moveTo" if format == Format::Word => {
            NodeKind::WordProtected
        }
        b"p" => NodeKind::Paragraph,
        b"pPr" => NodeKind::Properties,
        b"endParaRPr" => NodeKind::EndProperties,
        b"r" => NodeKind::Run,
        b"rPr" => NodeKind::RunProperties,
        b"vertAlign" | b"position" | b"vanish" | b"webHidden" if format == Format::Word => {
            NodeKind::ProtectedRunFormatting
        }
        b"noFill" if format == Format::PowerPoint => NodeKind::ProtectedRunFormatting,
        b"spcAft" => NodeKind::SpacingAfter,
        b"lnSpc" => NodeKind::LineSpacing,
        b"defRPr" => NodeKind::DefaultRunProperties,
        b"t" => NodeKind::Text,
        b"br" => NodeKind::Break,
        b"tab" | b"cr" | b"footnoteReference" | b"endnoteReference" | b"footnoteRef"
        | b"endnoteRef" | b"commentReference"
            if format == Format::Word =>
        {
            NodeKind::Break
        }
        b"hyperlink" if format == Format::Word => NodeKind::WordLink,
        b"fldSimple" if format == Format::Word => NodeKind::Field,
        b"fldChar" if format == Format::Word => NodeKind::WordFieldChar,
        b"instrText" if format == Format::Word => NodeKind::WordFieldText,
        b"delText" if format == Format::Word => NodeKind::WordDeletedText,
        b"bookmarkStart" | b"commentRangeStart" if format == Format::Word => {
            NodeKind::WordRangeStart
        }
        b"bookmarkEnd" | b"commentRangeEnd" if format == Format::Word => NodeKind::WordRangeEnd,
        b"proofErr" | b"lastRenderedPageBreak" if format == Format::Word => NodeKind::Proofing,
        b"fld" => NodeKind::Field,
        b"buChar" | b"buAutoNum" | b"buBlip" => NodeKind::Bullet,
        b"hlinkClick" | b"hlinkMouseOver" => NodeKind::Link,
        _ => NodeKind::Other,
    }
}

fn paragraphs(xml: &str) -> Result<(Vec<Paragraph>, usize), Error> {
    parse_paragraphs(xml, false)
}

fn parse_paragraphs(xml: &str, for_translation: bool) -> Result<(Vec<Paragraph>, usize), Error> {
    parse_office_paragraphs(xml, for_translation, Format::PowerPoint)
}

fn parse_office_paragraphs(
    xml: &str,
    for_translation: bool,
    format: Format,
) -> Result<(Vec<Paragraph>, usize), Error> {
    parse_paragraph_details(xml, for_translation, format)
        .map(|(paragraphs, skipped)| (paragraphs, skipped.len()))
}

struct RetainedParagraph {
    index: usize,
    in_table: bool,
    text: String,
    reason: RetentionReason,
}

fn visible_word_node(node: &Node, output: &mut String) {
    if matches!(node.kind, NodeKind::Text | NodeKind::WordDeletedText) {
        output.push_str(&node.text);
    }
    for child in &node.children {
        // Nested text boxes have their own translation/retention records.
        if !child.word_paragraph {
            visible_word_node(child, output);
        }
    }
}

fn retained_paragraph(
    xml: &str,
    node: &Node,
    index: usize,
    reason: RetentionReason,
) -> RetainedParagraph {
    fn toc(xml: &str, node: &Node) -> bool {
        (node.kind == NodeKind::WordFieldText
            && node
                .text
                .split_whitespace()
                .next()
                .is_some_and(|word| word.eq_ignore_ascii_case("TOC")))
            || (node.kind == NodeKind::Field
                && word_attribute(xml, node, b"instr").is_some_and(|value| {
                    value
                        .split(u8::is_ascii_whitespace)
                        .find(|word| !word.is_empty())
                        .is_some_and(|word| word.eq_ignore_ascii_case(b"TOC"))
                }))
            || node.children.iter().any(|child| toc(xml, child))
    }
    let mut text = String::new();
    visible_word_node(node, &mut text);
    text = text.split_whitespace().collect::<Vec<_>>().join(" ");
    if let Some((end, _)) = text.char_indices().nth(72) {
        text.truncate(end);
        text.push('…');
    }
    RetainedParagraph {
        index,
        in_table: node.in_word_table,
        text,
        reason: if toc(xml, node) {
            RetentionReason::TableOfContents
        } else {
            reason
        },
    }
}

// Ordinary unbound text controls are transparent at block and inline level.
// Keep placeholder/typed/bound/locked controls
// intact: Word may regenerate their display from data or a glossary entry.
fn ordinary_word_control(xml: &str) -> bool {
    let mut reader = quick_xml::Reader::from_str(xml);
    let mut depth: usize = 0;
    let mut properties = None;
    loop {
        let Ok(event) = reader.read_event() else {
            return false;
        };
        let opened = matches!(&event, Event::Start(_));
        match event {
            Event::Start(tag) | Event::Empty(tag) => {
                let name = tag.local_name();
                if depth == 1 && name.as_ref() == b"sdtPr" {
                    properties = Some(depth + 1);
                } else if properties == Some(depth)
                    && !matches!(
                        name.as_ref(),
                        b"rPr" | b"id" | b"alias" | b"tag" | b"placeholder" | b"text" | b"richText"
                    )
                {
                    return false;
                } else if depth == 1 && name.as_ref() == b"sdtContent" {
                    return true;
                }
                if opened {
                    depth += 1;
                }
            }
            Event::End(_) => {
                if properties == Some(depth) {
                    properties = None;
                }
                depth = depth.saturating_sub(1);
                if depth == 0 {
                    return false;
                }
            }
            Event::Eof => return false,
            _ => {}
        }
    }
}

fn parse_paragraph_details(
    xml: &str,
    for_translation: bool,
    format: Format,
) -> Result<(Vec<Paragraph>, Vec<RetainedParagraph>), Error> {
    let join_wrapped = for_translation && format == Format::PowerPoint;
    let mut reader = NsReader::from_str(xml);
    let mut stack: Vec<Node> = Vec::new();
    let mut nodes = Vec::new();
    let mut xml_depth = 0usize;
    let mut word_table_depth = 0usize;
    let mut word_locked_at = None;
    let mut word_field_depth = 0usize;
    let mut word_field_start = None;
    let mut word_fields = Vec::new();
    let mut word_marks: HashMap<_, usize> = HashMap::new();
    let mut word_intervals = Vec::new();
    let mut fallback_depth = 0usize;
    loop {
        let start = reader.buffer_position() as usize;
        let (ns, event) = reader.read_resolved_event().map_err(|_| Error::Xml)?;
        let word_element = format == Format::Word
            && matches!(&ns, ResolveResult::Bound(namespace) if [WORD, STRICT_WORD].contains(&namespace.as_ref()));
        let start_kind = match &event {
            Event::Start(e) | Event::Empty(e) => kind(ns, e.local_name().as_ref(), format),
            _ => NodeKind::Other,
        };
        // Keep the original fallback bytes in the drawing atom, but neither
        // translate nor report the duplicate compatibility representation.
        if fallback_depth > 0 || start_kind == NodeKind::WordFallback {
            match &event {
                Event::Start(_) => fallback_depth += 1,
                Event::End(_) => fallback_depth -= 1,
                Event::Eof => return Err(Error::Xml),
                Event::DocType(_) => return Err(Error::Xml),
                _ => {}
            }
            continue;
        }
        if matches!(&event, Event::Start(_)) {
            xml_depth += 1;
            if word_element
                && let Event::Start(e) = &event
                && e.local_name().as_ref() == b"tbl"
            {
                word_table_depth += 1;
            }
        }
        if word_element && let Event::Start(tag) | Event::Empty(tag) = &event {
            let name = tag.local_name();
            if matches!(
                start_kind,
                NodeKind::WordRangeStart | NodeKind::WordRangeEnd
            ) && let Some(id) = tag
                .attributes()
                .flatten()
                .find(|attr| attr.key.local_name().as_ref() == b"id")
            {
                let key = (
                    name.as_ref().starts_with(b"bookmark"),
                    id.value.into_owned(),
                );
                if start_kind == NodeKind::WordRangeStart {
                    if let Some(previous) = word_marks.insert(key, start) {
                        word_intervals.push((previous..start, false));
                    }
                } else if let Some(previous) = word_marks.remove(&key) {
                    word_intervals.push((previous..start, true));
                }
            }
            match name.as_ref() {
                b"fldChar" => {
                    match tag
                        .attributes()
                        .flatten()
                        .find(|attr| attr.key.local_name().as_ref() == b"fldCharType")
                        .map(|attr| attr.value)
                        .as_deref()
                    {
                        Some(b"begin") => {
                            if word_field_depth == 0 {
                                word_field_start = Some(start);
                            }
                            word_field_depth += 1;
                        }
                        Some(b"end") if word_field_depth > 0 => {
                            word_field_depth -= 1;
                            if word_field_depth == 0
                                && let Some(start) = word_field_start.take()
                            {
                                word_fields.push(start..reader.buffer_position() as usize);
                            }
                        }
                        _ => {}
                    }
                }
                b"ins" | b"del" | b"moveFrom" | b"moveTo" | b"sdt"
                    if matches!(&event, Event::Start(_))
                        && word_locked_at.is_none()
                        && (name.as_ref() != b"sdt"
                            || word_field_depth != 0
                            || !ordinary_word_control(&xml[start..])) =>
                {
                    word_locked_at = Some(xml_depth);
                }
                _ => {}
            }
        }
        if let Event::End(e) = &event {
            if word_element && e.local_name().as_ref() == b"tbl" {
                word_table_depth = word_table_depth.saturating_sub(1);
            }
            if word_locked_at == Some(xml_depth) {
                word_locked_at = None;
            }
            xml_depth = xml_depth.saturating_sub(1);
        }
        let end = reader.buffer_position() as usize;
        match event {
            Event::Start(_) if !stack.is_empty() || start_kind == NodeKind::Paragraph => {
                if stack.len() >= 64 {
                    return Err(Error::Xml);
                }
                stack.push(Node {
                    kind: if start_kind == NodeKind::Paragraph && word_locked_at.is_some() {
                        NodeKind::Other
                    } else {
                        start_kind
                    },
                    full: start..end,
                    body: end..end,
                    children: Vec::new(),
                    text: String::new(),
                    in_word_table: word_table_depth > 0,
                    word_paragraph: format == Format::Word && start_kind == NodeKind::Paragraph,
                });
            }
            Event::Empty(_) if !stack.is_empty() => {
                stack.last_mut().unwrap().children.push(Node {
                    kind: start_kind,
                    full: start..end,
                    body: end..end,
                    children: Vec::new(),
                    text: String::new(),
                    in_word_table: word_table_depth > 0,
                    word_paragraph: false,
                });
            }
            Event::End(_) if !stack.is_empty() => {
                let mut node = stack.pop().unwrap();
                node.full.end = end;
                node.body.end = start;
                if let Some(parent) = stack.last_mut() {
                    parent.children.push(node);
                } else {
                    nodes.push(node);
                }
            }
            Event::Text(event) => {
                if let Some(node) = stack.last_mut() {
                    node.text.push_str(&event.decode().map_err(|_| Error::Xml)?);
                }
            }
            Event::CData(event) => {
                if let Some(node) = stack.last_mut() {
                    node.text.push_str(&event.decode().map_err(|_| Error::Xml)?);
                }
            }
            Event::GeneralRef(event) if !stack.is_empty() => {
                let entity = event.decode().map_err(|_| Error::Xml)?;
                let escaped = format!("&{entity};");
                let value = quick_xml::escape::unescape(&escaped).map_err(|_| Error::Xml)?;
                stack.last_mut().unwrap().text.push_str(&value);
            }
            Event::DocType(_) => return Err(Error::Xml),
            Event::Eof if stack.is_empty() => break,
            Event::Eof => return Err(Error::Xml),
            _ => {}
        }
    }
    if let Some(start) = word_field_start {
        word_fields.push(start..xml.len());
    }
    // Keep the established outer-paragraph location numbering for result UI and
    // layout warnings. Text-box paragraphs share that location, but are separate
    // units in XML order, including when their enclosing paragraph is retained.
    fn nested_paragraphs(node: &Node, index: usize, out: &mut Vec<(usize, Node)>) {
        for child in &node.children {
            if child.word_paragraph {
                out.push((index, child.clone()));
            }
            nested_paragraphs(child, index, out);
        }
    }
    let mut located = Vec::new();
    for (index, node) in nodes.into_iter().enumerate() {
        located.push((index, node.clone()));
        if format == Format::Word {
            nested_paragraphs(&node, index, &mut located);
        }
    }
    let (locations, mut nodes): (Vec<_>, Vec<_>) = located.into_iter().unzip();

    // Bookmarks outside all paragraph text keep the same membership when that
    // text is replaced. Do not extend this to inline ranges or comment anchors.
    let valid_edges: HashSet<_> = word_intervals
        .iter()
        .filter(|(_, balanced)| *balanced)
        .flat_map(|(span, _)| [span.start, span.end])
        .collect();
    let mut edges = HashMap::new();
    for node in &nodes {
        let bookmark = |n: &Node| {
            valid_edges.contains(&n.full.start)
                && matches!(n.kind, NodeKind::WordRangeStart | NodeKind::WordRangeEnd)
                && xml[n.full.clone()]
                    .split_whitespace()
                    .next()
                    .is_some_and(|name| {
                        name.ends_with("bookmarkStart") || name.ends_with("bookmarkEnd")
                    })
        };
        let whitespace_run = |n: &Node| {
            n.kind == NodeKind::Run
                && n.children.iter().all(|c| match c.kind {
                    NodeKind::RunProperties | NodeKind::Proofing | NodeKind::Break => true,
                    NodeKind::Text => c.text.trim().is_empty(),
                    _ => false,
                })
        };
        let prefix: Vec<_> = node
            .children
            .iter()
            .take_while(|n| {
                bookmark(n)
                    || whitespace_run(n)
                    || matches!(n.kind, NodeKind::Properties | NodeKind::Proofing)
            })
            .collect();
        let suffix: Vec<_> = node
            .children
            .iter()
            .rev()
            .take_while(|n| {
                bookmark(n)
                    || whitespace_run(n)
                    || matches!(n.kind, NodeKind::EndProperties | NodeKind::Proofing)
            })
            .collect();
        for (group, kind) in [
            (prefix, NodeKind::WordPrefix),
            (suffix, NodeKind::WordSuffix),
        ] {
            if group.iter().any(|n| bookmark(n)) {
                for n in group {
                    if bookmark(n) || whitespace_run(n) {
                        edges.entry(n.full.start).or_insert(kind);
                    }
                }
            }
        }
    }
    // A local inline range must be handled as a pair by balanced_word_content.
    // Do not detach just its opening marker from the existing tagged path.
    for (span, balanced) in &word_intervals {
        if *balanced
            && nodes
                .iter()
                .any(|p| p.full.start <= span.start && p.full.end > span.end)
            && (!edges.contains_key(&span.start) || !edges.contains_key(&span.end))
        {
            edges.remove(&span.start);
            edges.remove(&span.end);
        }
    }

    // A complete paragraph strictly inside a balanced range cannot move outside
    // its endpoints when its body is replaced. Keep endpoint paragraphs intact
    // unless their range can be handled locally by balanced_word_content.
    // Malformed/unclosed ranges remain locked in their entirety.
    word_intervals.extend(
        word_marks
            .into_values()
            .map(|start| (start..xml.len(), false)),
    );
    let mut locks = vec![false; nodes.len()];
    for (span, balanced) in word_intervals {
        for (node, locked) in nodes.iter().zip(&mut locks) {
            if node.full.end <= span.start || node.full.start > span.end {
                continue;
            }
            if !balanced {
                *locked = true;
            } else if node.full.start > span.start || node.full.end <= span.end {
                // Only endpoint paragraphs are ambiguous. Fully enclosed text
                // keeps its range membership, including inside a text box.
                if (node.full.start <= span.start && !edges.contains_key(&span.start))
                    || (node.full.end > span.end && !edges.contains_key(&span.end))
                {
                    *locked = true;
                }
            }
        }
    }
    for node in &mut nodes {
        for child in &mut node.children {
            if let Some(kind) = edges.get(&child.full.start) {
                child.kind = *kind;
            }
        }
    }

    let mut out = Vec::new();
    let mut skipped = Vec::new();
    let mut nodes = locations
        .into_iter()
        .zip(nodes.into_iter().zip(locks))
        .peekable();
    while let Some((index, (mut node, locked))) = nodes.next() {
        if locked || node.kind != NodeKind::Paragraph {
            skipped.push(retained_paragraph(
                xml,
                &node,
                index + 1,
                if locked {
                    RetentionReason::Range
                } else {
                    RetentionReason::ProtectedStructure
                },
            ));
            continue;
        }
        if format == Format::Word {
            let content = cross_paragraph_word_fields(&node, &word_fields);
            match balanced_word_content(xml, &content) {
                Ok(content) => node.children = content,
                Err(()) => {
                    skipped.push(retained_paragraph(
                        xml,
                        &node,
                        index + 1,
                        RetentionReason::InlineStructure,
                    ));
                    continue;
                }
            }
        }
        let mut paragraph = Paragraph::from_node(xml, &node, for_translation, format);
        while join_wrapped
            && let Ok(Some(current)) = &paragraph
            && let Some((_, (next, false))) = nodes.peek()
            && let Some(joined) = join_paragraphs(xml, &node, current, next)
            && let Ok(Some(merged)) = Paragraph::from_node(xml, &joined, true, format)
        {
            nodes.next();
            node = joined;
            paragraph = Ok(Some(merged));
        }
        if format == Format::Word && paragraph.is_ok() {
            for child in &node.children {
                if matches!(child.kind, NodeKind::Field | NodeKind::WordProtected) {
                    let retained = retained_paragraph(
                        xml,
                        child,
                        index + 1,
                        RetentionReason::ProtectedStructure,
                    );
                    if retained.text.chars().any(char::is_alphabetic)
                        || retained.reason == RetentionReason::TableOfContents
                    {
                        skipped.push(retained);
                    }
                }
            }
        }
        match paragraph {
            Ok(Some(mut p)) => {
                if format == Format::Word {
                    p.word_location = Some((index + 1, node.in_word_table));
                }
                out.push(p);
            }
            Ok(None) => {}
            Err(()) => skipped.push(retained_paragraph(
                xml,
                &node,
                index + 1,
                RetentionReason::InlineStructure,
            )),
        }
    }
    Ok((out, skipped))
}

fn join_paragraphs(xml: &str, first: &Node, paragraph: &Paragraph, next: &Node) -> Option<Node> {
    // Only siblings in the same text body can be separated by XML whitespace alone.
    if !xml[first.full.end..next.full.start].trim().is_empty()
        || xml[first.full.start..first.body.start] != xml[next.full.start..next.body.start]
    {
        return None;
    }
    let properties = |node: &Node| {
        let props = node
            .children
            .iter()
            .find(|child| child.kind == NodeKind::Properties)?;
        let head = &xml[props.full.start..props.body.start];
        let formatting = props
            .children
            .iter()
            .filter(|child| !matches!(child.kind, NodeKind::SpacingAfter | NodeKind::LineSpacing))
            .map(|child| &xml[child.full.clone()])
            .collect::<Vec<_>>();
        Some((head, formatting))
    };
    if properties(first) != properties(next)
        || first.children.iter().chain(&next.children).any(|child| {
            matches!(child.kind, NodeKind::Break | NodeKind::Field)
                || child
                    .children
                    .iter()
                    .any(|child| child.kind == NodeKind::Bullet)
        })
    {
        return None;
    }

    let left_text = paragraph.plain_text();
    let next_paragraph = Paragraph::from_node(xml, next, true, Format::PowerPoint)
        .ok()
        .flatten()?;
    let right_text = next_paragraph.plain_text();
    let left =
        left_text.trim_end_matches(|c: char| c.is_whitespace() || "」』）】\"'”’".contains(c));
    let right = right_text.trim_start();
    let last = left.chars().next_back()?;
    let initial = right.chars().next()?;
    if "。！？.!?：:；;".contains(last)
        || "・•●○■□◆◇※（(［[".contains(initial)
        || left_text.len() + right_text.len() > 3600
    {
        return None;
    }
    let has_kana = |text: &str| text.chars().any(|c| matches!(c, 'ぁ'..='ヿ'));
    let same_style = paragraph.base.key == next_paragraph.base.key;
    let japanese_continuation = has_kana(left)
        && (has_kana(right) || matches!(initial, '一'..='龯'))
        && ("、をにでとがのはてたもへや".contains(last)
            || left.ends_with("たり")
            || left.ends_with("だから")
            || left.chars().count() >= 16);
    let latin_continuation = last.is_ascii() && initial.is_ascii_lowercase() && same_style;
    if !japanese_continuation && !latin_continuation {
        return None;
    }
    let mut joined = first.clone();
    if last.is_ascii()
        && initial.is_ascii_lowercase()
        && !left_text.ends_with(char::is_whitespace)
        && !right_text.starts_with(char::is_whitespace)
    {
        let text = joined
            .children
            .iter_mut()
            .rev()
            .filter(|child| child.kind == NodeKind::Run)
            .flat_map(|child| child.children.iter_mut())
            .find(|child| child.kind == NodeKind::Text && !child.text.is_empty())?;
        text.text.push(' ');
    }
    if let Some(properties) = joined
        .children
        .iter_mut()
        .find(|child| child.kind == NodeKind::Properties)
        && let Some(next_properties) = next
            .children
            .iter()
            .find(|child| child.kind == NodeKind::Properties)
    {
        // The merged paragraph ends where the last source paragraph ended.
        *properties = next_properties.clone();
    }
    joined.full.end = next.full.end;
    joined.body.end = next.body.end;
    let end = joined
        .children
        .iter()
        .position(|child| child.kind == NodeKind::EndProperties)
        .unwrap_or(joined.children.len());
    joined.children.splice(
        end..end,
        next.children
            .iter()
            .filter(|child| !matches!(child.kind, NodeKind::Properties | NodeKind::EndProperties))
            .cloned(),
    );
    Some(joined)
}

// Give a numeric expression that crosses formatting runs the style of its first digit.
fn keep_numbers_together(mut children: Vec<Child>) -> Vec<Child> {
    let mut output = Vec::with_capacity(children.len());
    let mut runs = Vec::new();
    let flush = |runs: &mut Vec<(Run, String)>, output: &mut Vec<Child>| {
        if runs.len() < 2 {
            output.extend(runs.drain(..).map(|(run, text)| Child::Run(run, text)));
            return;
        }
        let mut joined = String::new();
        let mut ends = Vec::with_capacity(runs.len());
        for (_, text) in runs.iter() {
            joined.push_str(text);
            ends.push(joined.len());
        }
        let mut position = 0;
        let mut emit = |to: usize, one_style: bool| {
            while position < to {
                let index = ends.partition_point(|&end| end <= position);
                let end = if one_style { to } else { to.min(ends[index]) };
                let style = if one_style {
                    let digit = joined[position..end]
                        .find(|c: char| c.is_ascii_digit() || ('０'..='９').contains(&c));
                    digit.map_or(index, |offset| {
                        ends.partition_point(|&boundary| boundary <= position + offset)
                    })
                } else {
                    index
                };
                let run = &runs[style].0;
                let fragment = &joined[position..end];
                if let Some(Child::Run(previous, text)) = output.last_mut()
                    && !previous.hyperlink
                    && previous.key == run.key
                {
                    text.push_str(fragment);
                } else {
                    output.push(Child::Run(run.clone(), fragment.to_owned()));
                }
                position = end;
            }
        };
        for range in amounts::expressions(&joined) {
            let index = ends.partition_point(|&end| end <= range.start);
            if range.end > ends[index] {
                emit(range.start, false);
                emit(range.end, true);
            }
        }
        emit(joined.len(), false);
        runs.clear();
    };
    for child in children.drain(..) {
        match child {
            Child::Run(run, text) if !run.hyperlink => runs.push((run, text)),
            other => {
                flush(&mut runs, &mut output);
                output.push(other);
            }
        }
    }
    flush(&mut runs, &mut output);
    output
}

// A field may span several paragraphs (e.g. a TOC). Preserve each intersecting
// group of complete children; text before its beginning or after its end is
// still editable. Never split a run or a control at a field boundary.
fn cross_paragraph_word_fields(node: &Node, fields: &[Range<usize>]) -> Vec<Node> {
    let fields: Vec<_> = fields
        .iter()
        .filter(|span| {
            span.start < node.body.end
                && node.body.start < span.end
                && (span.start < node.body.start || span.end > node.body.end)
        })
        .collect();
    let protected = |child: &Node| {
        !matches!(
            child.kind,
            NodeKind::Properties
                | NodeKind::EndProperties
                | NodeKind::WordPrefix
                | NodeKind::WordSuffix
        ) && fields
            .iter()
            .any(|span| span.start < child.full.end && child.full.start < span.end)
    };
    let mut result = Vec::new();
    let mut i = 0;
    while i < node.children.len() {
        let mut child = node.children[i].clone();
        if protected(&child) {
            let start = i;
            while i < node.children.len() && protected(&node.children[i]) {
                i += 1;
            }
            child.kind = NodeKind::Field;
            child.full.end = node.children[i - 1].full.end;
            child.children = node.children[start..i].to_vec();
        } else {
            i += 1;
        }
        result.push(child);
    }
    result
}

fn word_attribute(xml: &str, node: &Node, key: &[u8]) -> Option<Vec<u8>> {
    let mut reader = quick_xml::Reader::from_str(&xml[node.full.clone()]);
    let (Event::Start(tag) | Event::Empty(tag)) = reader.read_event().ok()? else {
        return None;
    };
    tag.attributes()
        .flatten()
        .find(|a| a.key.local_name().as_ref() == key)
        .map(|a| a.value.into_owned())
}

fn word_boundary(template: &Node, range: Range<usize>) -> Node {
    let mut node = template.clone();
    node.kind = NodeKind::WordBoundary;
    node.body = range.clone();
    node.full = range;
    node.children.clear();
    node.text.clear();
    node.word_paragraph = false;
    node
}

// The opening and closing XML are ordered opaque atoms. Runs between them use
// the same paragraph context, without duplicating control IDs or properties.
fn word_container(
    xml: &str,
    node: &Node,
    body: Range<usize>,
    contents: &[Node],
    out: &mut Vec<Node>,
) -> Result<(), ()> {
    out.push(word_boundary(node, node.full.start..body.start));
    let mut content = balanced_word_content(xml, contents)?;
    // Require text to remain between its own control/link boundaries. Making
    // scoped runs explicit style tags prevents a model from moving their words
    // outside a control while still returning both boundary atoms.
    for child in &mut content {
        if child.kind == NodeKind::Run {
            child.kind = NodeKind::WordScopedRun;
        }
    }
    out.extend(content);
    out.push(word_boundary(node, body.end..node.full.end));
    Ok(())
}

fn balanced_word_content(xml: &str, contents: &[Node]) -> Result<Vec<Node>, ()> {
    let mut result = Vec::new();
    let mut i = 0;
    let field_type = |run: &Node| -> Option<Vec<u8>> {
        if !matches!(run.kind, NodeKind::Run | NodeKind::WordScopedRun) {
            return None;
        }
        let node = run
            .children
            .iter()
            .find(|n| n.kind == NodeKind::WordFieldChar)?;
        word_attribute(xml, node, b"fldCharType")
    };
    while i < contents.len() {
        let node = &contents[i];
        if node.kind == NodeKind::WordControl {
            if ordinary_word_control(&xml[node.full.clone()]) {
                let mut bodies = node
                    .children
                    .iter()
                    .filter(|n| n.kind == NodeKind::WordControlContent);
                let body = bodies.next().ok_or(())?;
                if bodies.next().is_some() {
                    return Err(());
                }
                word_container(xml, node, body.body.clone(), &body.children, &mut result)?;
            } else {
                let mut protected = node.clone();
                protected.kind = NodeKind::WordProtected;
                result.push(protected);
            }
            i += 1;
        } else if (node.kind == NodeKind::Field
            && word_attribute(xml, node, b"instr").is_some_and(|value| {
                value
                    .split(u8::is_ascii_whitespace)
                    .find(|word| !word.is_empty())
                    .is_some_and(|word| word.eq_ignore_ascii_case(b"HYPERLINK"))
            }))
            || (node.kind == NodeKind::WordLink
                && !matches!(node.children.as_slice(), [run] if run.kind == NodeKind::Run))
        {
            word_container(xml, node, node.body.clone(), &node.children, &mut result)?;
            i += 1;
        } else if node.kind == NodeKind::WordRangeStart {
            let end = if contents
                .get(i + 1)
                .is_some_and(|n| n.kind == NodeKind::WordRangeEnd)
            {
                i + 1
            } else {
                i + 2
            };
            let close = contents
                .get(end)
                .filter(|n| n.kind == NodeKind::WordRangeEnd)
                .ok_or(())?;
            let bookmark = |marker: &Node| {
                xml[marker.full.clone()]
                    .split_whitespace()
                    .next()
                    .is_some_and(|name| {
                        name.ends_with("bookmarkStart") || name.ends_with("bookmarkEnd")
                    })
            };
            if bookmark(node) != bookmark(close)
                || word_attribute(xml, node, b"id")
                    .zip(word_attribute(xml, close, b"id"))
                    .is_none_or(|(a, b)| a != b)
                || (end == i + 2 && contents[i + 1].kind != NodeKind::Run)
            {
                return Err(());
            }
            let mut wrapper = node.clone();
            wrapper.full.end = close.full.end;
            wrapper.kind = if end == i + 1 {
                NodeKind::Field
            } else {
                wrapper.children.push(contents[i + 1].clone());
                NodeKind::WordRange
            };
            result.push(wrapper);
            i = end + 1;
        } else if field_type(node).as_deref() == Some(b"begin") {
            let mut depth = 1usize;
            let mut end = i + 1;
            let mut separator = None;
            let mut nested = false;
            let mut instruction = String::new();
            while depth != 0 {
                let run = contents.get(end).ok_or(())?;
                if separator.is_none() {
                    for child in &run.children {
                        if child.kind == NodeKind::WordFieldText {
                            instruction.push_str(&child.text);
                        }
                    }
                }
                match field_type(run).as_deref() {
                    Some(b"begin") => {
                        depth += 1;
                        nested = true;
                    }
                    Some(b"end") => depth -= 1,
                    Some(b"separate") if depth == 1 && separator.replace(end).is_some() => {
                        return Err(());
                    }
                    _ => {}
                }
                end += 1;
            }
            let mut wrapper = node.clone();
            wrapper.full.end = contents[end - 1].full.end;
            wrapper.kind = NodeKind::Field;
            wrapper.children = contents[i..end].to_vec();
            if !nested
                && instruction
                    .split_whitespace()
                    .next()
                    .is_some_and(|word| word.eq_ignore_ascii_case("HYPERLINK"))
                && let Some(separator) = separator
                && [i, separator, end - 1].iter().all(|&at| {
                    contents[at].children.iter().all(|n| {
                        matches!(n.kind, NodeKind::WordFieldChar | NodeKind::RunProperties)
                    })
                })
            {
                let body = contents[separator].full.end..contents[end - 1].full.start;
                word_container(
                    xml,
                    &wrapper,
                    body,
                    &contents[separator + 1..end - 1],
                    &mut result,
                )?;
            } else {
                result.push(wrapper);
            }
            i = end;
        } else {
            result.push(node.clone());
            i += 1;
        }
    }
    Ok(result)
}

fn is_japanese(ch: char) -> bool {
    matches!(ch, 'ぁ'..='ゖ' | 'ァ'..='ヺ' | '㐀'..='䶿' | '一'..='鿿')
}

// Text-box paragraphs are extracted independently. Other drawing text (e.g.
// DrawingML chart labels) still prevents a silent claim of full translation.
fn supported_word_drawing(node: &Node) -> bool {
    if node.kind == NodeKind::WordTextBox {
        return true;
    }
    !matches!(node.kind, NodeKind::Text | NodeKind::Paragraph)
        && (node.text.trim().is_empty() || node.text.trim().parse::<f64>().is_ok())
        && node.children.iter().all(supported_word_drawing)
}

fn split_word_run(xml: &str, run: &Node, result: &mut Vec<Child>) -> Result<(), ()> {
    let props = run
        .children
        .first()
        .filter(|p| p.kind == NodeKind::RunProperties);
    let heading = &xml[run.full.start..run.body.start];
    let closing = &xml[run.body.end..run.full.end];
    let properties = props.map_or("", |p| &xml[p.full.clone()]);
    let key = props
        .map(|_| rewrite_properties(properties, None))
        .transpose()
        .map_err(|_| ())?
        .unwrap_or_default();
    for item in &run.children[usize::from(props.is_some())..] {
        match item.kind {
            NodeKind::Proofing => {}
            NodeKind::WordDrawing if supported_word_drawing(item) => {
                result.push(Child::Embedded(
                    format!("{heading}{properties}{}{closing}", &xml[item.full.clone()]),
                    item.full.clone(),
                    heading.len() + properties.len(),
                ));
            }
            NodeKind::Break => result.push(Child::Atom(format!(
                "{heading}{properties}{}{closing}",
                &xml[item.full.clone()]
            ))),
            NodeKind::Text
                if item.children.is_empty()
                    && item.body.start != item.full.end
                    && !["<g", "</g", "<x", "</x", "<a i=", "</a>"]
                        .iter()
                        .any(|prefix| item.text.contains(prefix)) =>
            {
                let opening = layout::rewrite_tag(
                    &xml[item.full.start..item.body.start],
                    &[(b"xml:space", "preserve")],
                )
                .map_err(|_| ())?;
                result.push(Child::Run(
                    Run {
                        write_before: format!("{heading}{properties}{opening}"),
                        write_after: format!("{}{closing}", &xml[item.body.end..item.full.end]),
                        key: key.clone(),
                        props: None,
                        hyperlink: run.kind == NodeKind::WordScopedRun,
                        word: true,
                    },
                    item.text.clone(),
                ));
            }
            _ => return Err(()),
        }
    }
    Ok(())
}

// Check original properties before amount protection can combine adjacent runs.
fn has_protected_run_formatting(xml: &str, node: &Node, format: Format) -> bool {
    node.children.iter().any(|child| match child.kind {
        NodeKind::Paragraph => false,
        NodeKind::RunProperties => {
            child
                .children
                .iter()
                .any(|n| n.kind == NodeKind::ProtectedRunFormatting)
                || (format == Format::PowerPoint
                    && matches!(
                        Reader::from_str(&xml[child.full.start..child.body.start]).read_event(),
                        Ok(Event::Start(tag) | Event::Empty(tag))
                            if tag.attributes().flatten().any(|a|
                                a.key.as_ref() == b"baseline"
                                    && !matches!(a.value.as_ref(), b"0" | b"0%"))
                    ))
        }
        _ => has_protected_run_formatting(xml, child, format),
    })
}

impl Paragraph {
    fn from_node(
        xml: &str,
        node: &Node,
        protect_amounts: bool,
        format: Format,
    ) -> Result<Option<Self>, ()> {
        let mut before = String::new();
        let mut after = String::new();
        let mut children = Vec::new();
        let content = &node.children;
        let mut ordered_tags = format == Format::Word
            && content.iter().any(|node| {
                matches!(
                    node.kind,
                    NodeKind::Field
                        | NodeKind::WordRange
                        | NodeKind::WordBoundary
                        | NodeKind::WordProtected
                )
            });
        for child in content {
            let full = &xml[child.full.clone()];
            match child.kind {
                NodeKind::Properties | NodeKind::WordPrefix => before.push_str(full),
                NodeKind::Proofing => {}
                NodeKind::EndProperties | NodeKind::WordSuffix => after.push_str(full),
                NodeKind::Break
                | NodeKind::Field
                | NodeKind::WordBoundary
                | NodeKind::WordProtected => children.push(Child::Atom(full.to_owned())),
                NodeKind::WordDrawing if supported_word_drawing(child) => {
                    children.push(Child::Embedded(full.to_owned(), child.full.clone(), 0));
                }
                NodeKind::Run | NodeKind::WordScopedRun
                    if format == Format::Word
                        && child
                            .children
                            .iter()
                            .filter(|n| n.kind != NodeKind::RunProperties)
                            .count()
                            > 1 =>
                {
                    split_word_run(xml, child, &mut children)?;
                    ordered_tags |= child.children.iter().any(|n| n.kind == NodeKind::Break);
                }
                NodeKind::Run
                | NodeKind::WordScopedRun
                | NodeKind::WordLink
                | NodeKind::WordRange => {
                    let word = format == Format::Word;
                    let run_node = if matches!(child.kind, NodeKind::WordLink | NodeKind::WordRange)
                    {
                        match child.children.as_slice() {
                            [run] if run.kind == NodeKind::Run => run,
                            _ => return Err(()),
                        }
                    } else {
                        child
                    };
                    if word
                        && matches!(child.kind, NodeKind::Run | NodeKind::WordScopedRun)
                        && run_node
                            .children
                            .iter()
                            .any(|n| matches!(n.kind, NodeKind::Break | NodeKind::WordDrawing))
                        && run_node.children.iter().all(|n| {
                            matches!(n.kind, NodeKind::Break | NodeKind::RunProperties)
                                || (n.kind == NodeKind::WordDrawing && supported_word_drawing(n))
                        })
                    {
                        // A Word run may contain a protected drawing, tab, or reference.
                        children.push(Child::Embedded(full.to_owned(), child.full.clone(), 0));
                        continue;
                    }
                    let Some(text) = run_node.children.iter().find(|c| c.kind == NodeKind::Text)
                    else {
                        return Err(());
                    };
                    if ["<g", "</g", "<x", "</x", "<a i=", "</a>"]
                        .iter()
                        .any(|prefix| text.text.contains(prefix))
                        || !text.children.is_empty()
                        || text.body.start == text.full.end
                        || run_node
                            .children
                            .iter()
                            .any(|c| !matches!(c.kind, NodeKind::Text | NodeKind::RunProperties))
                    {
                        return Err(());
                    }
                    let props = match run_node.children.as_slice() {
                        [only_text] if only_text.kind == NodeKind::Text => None,
                        [props, text]
                            if props.kind == NodeKind::RunProperties
                                && text.kind == NodeKind::Text =>
                        {
                            Some(props)
                        }
                        _ => return Err(()),
                    };
                    let mut write_before = xml[child.full.start..text.full.start].to_owned();
                    let text_open = &xml[text.full.start..text.body.start];
                    if word {
                        write_before.push_str(
                            &layout::rewrite_tag(text_open, &[(b"xml:space", "preserve")])
                                .map_err(|_| ())?,
                        );
                    } else {
                        write_before.push_str(text_open);
                    }
                    let run = Run {
                        write_before,
                        write_after: xml[text.body.end..child.full.end].to_owned(),
                        key: props
                            .map(|p| rewrite_properties(&xml[p.full.clone()], None))
                            .transpose()
                            .map_err(|_| ())?
                            .unwrap_or_default(),
                        props: props.map(|p| {
                            p.full.start - child.full.start..p.full.end - child.full.start
                        }),
                        hyperlink: matches!(
                            child.kind,
                            NodeKind::WordLink | NodeKind::WordRange | NodeKind::WordScopedRun
                        ) || props
                            .is_some_and(|p| p.children.iter().any(|n| n.kind == NodeKind::Link)),
                        word,
                    };
                    if let Some(Child::Run(prev, text_before)) = children.last_mut()
                        && !prev.hyperlink
                        && !run.hyperlink
                        && prev.key == run.key
                    {
                        text_before.push_str(&text.text);
                    } else {
                        children.push(Child::Run(run, text.text.clone()));
                    }
                }
                _ => return Err(()),
            }
        }
        if protect_amounts {
            children = keep_numbers_together(children);
        }
        let has_text = children
            .iter()
            .any(|c| matches!(c, Child::Run(_, text) if text.chars().any(char::is_alphabetic)));
        if !has_text {
            return Ok(None);
        }
        let mut weights: Vec<(&Run, usize)> = Vec::new();
        for child in &children {
            if let Child::Run(run, text) = child
                && !run.hyperlink
            {
                let n = text.chars().count();
                if let Some((_, count)) = weights.iter_mut().find(|(style, _)| style.key == run.key)
                {
                    *count += n;
                } else {
                    weights.push((run, n));
                }
            }
        }
        let base = weights
            .into_iter()
            .rev()
            .max_by_key(|(_, weight)| *weight)
            .map(|(run, _)| run.clone());
        let mut tags = Vec::new();
        let mut embedded_atoms = Vec::new();
        let mut items = Vec::new();
        for child in children {
            match child {
                Child::Run(_, text) if text.is_empty() => {}
                Child::Run(run, text)
                    if text.trim().is_empty()
                        && (run.hyperlink || base.as_ref().is_none_or(|b| b.key != run.key)) =>
                {
                    // Spaces in a distinct font or width are layout elements.
                    let original =
                        format!("{}{}{}", run.write_before, escape(&text), run.write_after);
                    items.push(Item::Atom(tags.len()));
                    tags.push(Tag::Atom(original));
                }
                Child::Run(run, text)
                    if !run.hyperlink && base.as_ref().is_some_and(|b| b.key == run.key) =>
                {
                    items.push(Item::Text(text, None))
                }
                Child::Run(run, text) => {
                    items.push(Item::Text(text, Some(tags.len())));
                    tags.push(Tag::Style(run));
                }
                Child::Atom(xml) => {
                    items.push(Item::Atom(tags.len()));
                    tags.push(Tag::Atom(xml));
                }
                Child::Embedded(xml, source, offset) => {
                    embedded_atoms.push(EmbeddedAtom {
                        tag: tags.len(),
                        source,
                        offset,
                    });
                    items.push(Item::Atom(tags.len()));
                    tags.push(Tag::Atom(xml));
                }
            }
        }
        // For link-only paragraphs, only the tagged link can supply a run template.
        let base = base
            .or_else(|| {
                tags.iter().find_map(|t| match t {
                    Tag::Style(run) => Some(run.clone()),
                    _ => None,
                })
            })
            .ok_or(())?;
        let font_size = node
            .children
            .iter()
            .flat_map(|child| &child.children)
            .filter(|child| {
                matches!(
                    child.kind,
                    NodeKind::RunProperties | NodeKind::DefaultRunProperties
                )
            })
            .filter_map(|child| layout::font_points(&xml[child.full.clone()]))
            .max_by(f64::total_cmp);
        Ok(Some(Self {
            body: node.body.clone(),
            font_size,
            inherited_font_size: None,
            inherited_ea: None,
            ordered_tags,
            protected_run_formatting: has_protected_run_formatting(xml, node, format),
            word_location: None,
            before,
            after,
            base,
            tags,
            embedded_atoms,
            items,
        }))
    }

    pub fn word_location(&self) -> Option<(usize, bool)> {
        self.word_location
    }

    pub fn word_translation_needs_review(
        &self,
        format: TagFormat,
        translated: &str,
        language: &str,
    ) -> bool {
        if self.word_location.is_none() {
            return false;
        }
        let translated = self.repair_word_atom_close(format, translated);
        let Ok(visible) = self.visible_target(format, &translated) else {
            return true;
        };
        let original = self.plain_text();
        if original.trim() == visible.trim() {
            return original.split_whitespace().count() > 1 || original.chars().any(is_japanese);
        }
        match language {
            "en" => visible.chars().filter(|&ch| is_japanese(ch)).count() >= 3,
            "ja" => {
                let original = original
                    .split(|ch: char| !ch.is_ascii_alphabetic())
                    .filter(|word| word.len() >= 3)
                    .collect::<HashSet<_>>();
                let mut consecutive = 0;
                visible
                    .split(|ch: char| !ch.is_ascii_alphabetic())
                    .filter(|word| !word.is_empty())
                    .any(|word| {
                        if original.contains(word) {
                            consecutive += 1;
                        } else {
                            consecutive = 0;
                        }
                        consecutive >= 2
                    })
            }
            _ => false,
        }
    }

    /// Byte range of this unit in the source package's paragraph XML.
    #[cfg(feature = "probe-inference")]
    pub fn source_xml_range(&self) -> Range<usize> {
        self.body.clone()
    }

    pub fn has_tags(&self) -> bool {
        !self.tags.is_empty()
    }

    pub fn plain_text(&self) -> String {
        self.items
            .iter()
            .map(|item| match item {
                Item::Text(s, _) => s.as_str(),
                Item::Atom(_) => " ",
            })
            .collect()
    }

    pub fn source(&self, format: TagFormat) -> String {
        let mut text = String::new();
        for item in &self.items {
            match item {
                Item::Text(s, None) => text.push_str(s),
                Item::Text(s, Some(id)) => text.push_str(&format.wrap(id + 1, s, false)),
                Item::Atom(id) => text.push_str(&format.wrap(id + 1, "", true)),
            }
        }
        text
    }

    pub fn valid(&self, format: TagFormat, text: &str) -> bool {
        self.rebuild(format, text, "").is_ok()
    }

    /// The text must be contiguous between any leading/trailing atoms. An atom between
    /// text runs has no provable position in the translated text. Linked and scoped runs
    /// require their tags to preserve the original target or range. Baseline and visibility
    /// properties carry meaning that a paragraph-wide style cannot preserve.
    fn fallback_text_range(&self) -> Option<Range<usize>> {
        if self.tags.is_empty()
            || self.protected_run_formatting
            || self.base.hyperlink
            || self
                .tags
                .iter()
                .any(|tag| matches!(tag, Tag::Style(run) if run.hyperlink))
        {
            return None;
        }
        let is_text = |item: &Item| matches!(item, Item::Text(..));
        let start = self.items.iter().position(is_text)?;
        let end = self.items.iter().rposition(is_text)? + 1;
        self.items[start..end]
            .iter()
            .all(|item| {
                matches!(item, Item::Text(text, _) if !text.contains(|c| {
                    // A private-use glyph can depend on its run's symbol font.
                    matches!(c, '\r' | '\n' | '\t' | '◇'
                        | '\u{e000}'..='\u{f8ff}' | '\u{f0000}'..='\u{ffffd}'
                        | '\u{100000}'..='\u{10fffd}')
                }))
            })
            .then_some(start..end)
    }

    fn anchor_fallback_source(&self) -> Option<String> {
        self.fallback_text_range()?;
        Some(self.plain_text().trim().to_owned())
    }

    fn target_text<'a>(
        &self,
        format: TagFormat,
        target: &'a OfficeTarget,
    ) -> Result<Cow<'a, str>, Error> {
        let text = match target {
            OfficeTarget::Tagged(text) => return Ok(Cow::Borrowed(text)),
            OfficeTarget::Simplified(text) => text,
        };
        let range = self.fallback_text_range().ok_or(Error::Translation)?;
        let text = text.trim();
        if text.is_empty() || text.contains(['\r', '\n', '\t', '◇']) || format.next(text)?.is_some()
        {
            return Err(Error::Translation);
        }
        let mut target = String::new();
        for item in &self.items[..range.start] {
            let Item::Atom(id) = item else { unreachable!() };
            target.push_str(&format.wrap(id + 1, "", true));
        }
        target.push_str(text);
        for item in &self.items[range.end..] {
            let Item::Atom(id) = item else { unreachable!() };
            target.push_str(&format.wrap(id + 1, "", true));
        }
        Ok(Cow::Owned(target))
    }

    /// Accept a fallback only when it can be rendered, retains quantities, and changes text.
    pub fn simplified_target(
        &self,
        format: TagFormat,
        text: String,
        language: &str,
    ) -> Option<OfficeTarget> {
        let target = OfficeTarget::Simplified(text);
        let target_text = self.target_text(format, &target).ok()?;
        self.rebuild_impl(format, &target_text, language, &[], true)
            .ok()?;
        (self.amounts_match(format, &target_text, language)
            && self.changes_text(format, &target_text, language))
        .then_some(target)
    }

    pub fn target_amounts_match(
        &self,
        format: TagFormat,
        target: &OfficeTarget,
        language: &str,
    ) -> bool {
        self.target_text(format, target)
            .is_ok_and(|text| self.amounts_match(format, &text, language))
    }

    pub fn target_changes_text(
        &self,
        format: TagFormat,
        target: &OfficeTarget,
        language: &str,
    ) -> bool {
        self.target_text(format, target)
            .is_ok_and(|text| self.changes_text(format, &text, language))
    }

    fn visible_office_target(
        &self,
        format: TagFormat,
        target: &OfficeTarget,
    ) -> Result<String, Error> {
        self.visible_target(format, &self.target_text(format, target)?)
    }

    /// Counts a change only after the same repair and validation used for export.
    pub fn changes_text(&self, format: TagFormat, text: &str, language: &str) -> bool {
        let text = self.repair_word_atom_close(format, text);
        self.with_word_spaces(format, &text, language)
            .and_then(|text| self.visible_target(format, &text))
            .is_ok_and(|visible| {
                !visible.trim().is_empty() && visible.trim() != self.plain_text().trim()
            })
    }

    pub fn amounts_match(&self, format: TagFormat, text: &str, language: &str) -> bool {
        let text = self.repair_word_atom_close(format, text);
        self.with_word_spaces(format, &text, language)
            .and_then(|text| self.visible_target(format, &text))
            .is_ok_and(|visible| amounts::matches(&self.plain_text(), &visible, language))
    }

    // The model sometimes closes an ordinary style twice before resuming at a
    // protected Word atom. Remove only the surplus close. The regular parser
    // still requires every original tag and validates each tag's contents.
    fn repair_word_atom_close<'a>(
        &self,
        format: TagFormat,
        text: &'a str,
    ) -> std::borrow::Cow<'a, str> {
        use std::borrow::Cow;
        // An ordinary style may be left open immediately before the next style.
        // Repair only one missing close in a style-only Word paragraph. Links,
        // fields, drawings, unknown/duplicate IDs and any remaining malformed
        // structure are still rejected by the normal rebuild validation.
        if self.base.word
            && matches!(format, TagFormat::Anchor)
            && self
                .tags
                .iter()
                .all(|tag| matches!(tag, Tag::Style(run) if !run.hyperlink))
        {
            let opens: Vec<_> = text.match_indices("<a i=\"").map(|(i, _)| i).collect();
            if opens.len() == self.tags.len() && text.matches("</a>").count() + 1 == opens.len() {
                let missing: Vec<_> = opens
                    .windows(2)
                    .filter(|pair| !text[pair[0]..pair[1]].contains("</a>"))
                    .map(|pair| pair[1])
                    .collect();
                if let [at] = missing.as_slice() {
                    let mut corrected = text.to_owned();
                    corrected.insert_str(*at, "</a>");
                    return Cow::Owned(corrected);
                }
            }
        }
        if !self.base.word
            || !matches!(format, TagFormat::Anchor)
            || !self.tags.iter().any(|tag| matches!(tag, Tag::Atom(_)))
        {
            return Cow::Borrowed(text);
        }

        let mut offset = 0;
        let mut previous_style = None;
        loop {
            let rest = &text[offset..];
            match format.next(rest) {
                Ok(Some(tag)) => {
                    previous_style =
                        matches!(self.tags.get(tag.id), Some(Tag::Style(run)) if !run.hyperlink)
                            .then_some(tag.id);
                    offset += tag.end;
                }
                Ok(None) => break,
                Err(_) => {
                    let Some(id) = previous_style else { break };
                    let Some(close) = rest.find("</a>") else {
                        break;
                    };
                    let Some(after) = rest.get(close + "</a>".len()..) else {
                        break;
                    };
                    if rest[..close].contains(['<', '>']) {
                        break;
                    }
                    let following = after.trim_start();
                    if !matches!(format.next(following), Ok(Some(tag))
                        if tag.start == 0
                            && tag.id == id + 1
                            && tag.body == format.atom_body()
                            && matches!(self.tags.get(tag.id), Some(Tag::Atom(_))))
                    {
                        break;
                    }
                    let mut corrected = text.to_owned();
                    corrected.replace_range(offset + close..offset + close + "</a>".len(), "");
                    return Cow::Owned(corrected);
                }
            }
        }
        Cow::Borrowed(text)
    }

    fn with_word_spaces<'a>(
        &self,
        format: TagFormat,
        text: &'a str,
        language: &str,
    ) -> Result<std::borrow::Cow<'a, str>, Error> {
        use std::borrow::Cow;
        if !self.base.word || language != "en" || self.tags.is_empty() {
            return Ok(Cow::Borrowed(text));
        }

        // A style boundary between contiguous Latin letters may be inside a name or identifier.
        // Source spaces and Japanese text provide evidence for a word boundary in English.
        let edge = |item: &Item, first: bool| match item {
            Item::Text(text, _) if first => text.chars().next(),
            Item::Text(text, _) => text.chars().next_back(),
            Item::Atom(_) => None,
        };
        let can_separate = |left: Option<char>, right: Option<char>| {
            left.zip(right).is_some_and(|(left, right)| {
                [left, right].iter().any(|&c| {
                    c.is_whitespace() || matches!(c, 'ぁ'..='ヿ' | '㐀'..='䶿' | '一'..='鿿')
                })
            })
        };
        let mut boundaries = vec![(false, false); self.tags.len()];
        for (index, item) in self.items.iter().enumerate() {
            if let Item::Text(_, Some(id)) = item {
                boundaries[*id] = (
                    can_separate(
                        index
                            .checked_sub(1)
                            .and_then(|i| edge(&self.items[i], false)),
                        edge(item, true),
                    ),
                    can_separate(
                        edge(item, false),
                        self.items.get(index + 1).and_then(|item| edge(item, true)),
                    ),
                );
            }
        }

        let mut insertions = Vec::new();
        let mut previous = None::<(u8, bool)>;
        let mut observe =
            |fragment: &str, offset, (before, after), previous: &mut Option<(u8, bool)>| {
                if let (Some(&first), Some(&last)) =
                    (fragment.as_bytes().first(), fragment.as_bytes().last())
                {
                    if first.is_ascii_alphabetic()
                        && previous.is_some_and(|(last, boundary)| {
                            last.is_ascii_alphabetic() && (boundary || before)
                        })
                    {
                        insertions.push(offset);
                    }
                    *previous = Some((last, after));
                }
            };
        let mut offset = 0;
        while let Some(tag) = format.next(&text[offset..])? {
            observe(
                &text[offset..offset + tag.start],
                offset,
                (false, false),
                &mut previous,
            );
            match (self.tags.get(tag.id), tag.body) {
                (Some(Tag::Style(_)), Some(body)) => {
                    observe(
                        body,
                        offset + tag.body_start,
                        boundaries[tag.id],
                        &mut previous,
                    );
                }
                _ => previous = None,
            }
            offset += tag.end;
        }
        observe(&text[offset..], offset, (false, false), &mut previous);
        if insertions.is_empty() {
            return Ok(Cow::Borrowed(text));
        }
        let mut spaced = String::with_capacity(text.len() + insertions.len());
        let mut start = 0;
        for insertion in insertions {
            spaced.push_str(&text[start..insertion]);
            spaced.push(' ');
            start = insertion;
        }
        spaced.push_str(&text[start..]);
        Ok(Cow::Owned(spaced))
    }

    fn visible_target(&self, format: TagFormat, mut target: &str) -> Result<String, Error> {
        let mut out = String::new();
        while let Some(tag) = format.next(target)? {
            out.push_str(&target[..tag.start]);
            match self.tags.get(tag.id) {
                Some(Tag::Style(_)) => out.push_str(tag.body.unwrap_or_default()),
                Some(Tag::Atom(xml))
                    if xml.split_once('<').is_some_and(|(_, tag)| {
                        tag.split([':', ' ', '/', '>'])
                            .take(2)
                            .any(|part| part == "br")
                    }) =>
                {
                    out.push('\n')
                }
                _ => out.push(' '),
            }
            target = &target[tag.end..];
        }
        out.push_str(target);
        Ok(out)
    }

    fn rebuild(
        &self,
        format: TagFormat,
        text: &str,
        target_language: &str,
    ) -> Result<String, Error> {
        self.rebuild_impl(format, text, target_language, &[], false)
    }

    fn rebuild_target(
        &self,
        format: TagFormat,
        target: &OfficeTarget,
        target_language: &str,
        edits: &[(Range<usize>, String)],
    ) -> Result<String, Error> {
        let text = self.target_text(format, target)?;
        self.rebuild_impl(
            format,
            &text,
            target_language,
            edits,
            matches!(target, OfficeTarget::Simplified(_)),
        )
    }

    fn rebuild_impl(
        &self,
        format: TagFormat,
        text: &str,
        target_language: &str,
        edits: &[(Range<usize>, String)],
        simplified: bool,
    ) -> Result<String, Error> {
        // Every nested edit must belong to exactly one preserved atom. Reject
        // unexpected overlap instead of discarding a child's translation.
        for (range, _) in edits {
            if self.body.start <= range.start
                && range.end <= self.body.end
                && self
                    .embedded_atoms
                    .iter()
                    .filter(|atom| atom.source.start <= range.start && range.end <= atom.source.end)
                    .count()
                    != 1
            {
                return Err(Error::Translation);
            }
        }
        if text.trim().is_empty() {
            return Err(Error::Translation);
        }
        let repaired = self.repair_word_atom_close(format, text);
        let text = self.with_word_spaces(format, &repaired, target_language)?;
        let mut result = self.before.clone();
        let mut seen = vec![false; self.tags.len()];
        let mut rest = text.as_ref();
        let mut previous = None;
        while let Some(ParsedTag {
            start,
            id,
            body,
            end,
            ..
        }) = format.next(rest)?
        {
            self.base.write(
                &mut result,
                &rest[..start],
                !self.base.hyperlink,
                target_language,
            )?;
            let tag = self.tags.get(id).ok_or(Error::Translation)?;
            if self.ordered_tags && previous.is_some_and(|old| id <= old) {
                return Err(Error::Translation);
            }
            previous = Some(id);
            if std::mem::replace(&mut seen[id], true) {
                return Err(Error::Translation);
            }
            match tag {
                Tag::Style(run) if body.is_some_and(|s| !s.trim().is_empty() && s != "◇") => {
                    run.write(&mut result, body.unwrap(), true, target_language)?;
                }
                Tag::Atom(xml) if body == format.atom_body() => {
                    let mut xml = xml.clone();
                    if let Some(atom) = self.embedded_atoms.iter().find(|atom| atom.tag == id) {
                        let mut descendants: Vec<_> = edits
                            .iter()
                            .filter(|(r, _)| {
                                atom.source.start <= r.start && r.end <= atom.source.end
                            })
                            .collect();
                        descendants.sort_unstable_by_key(|(r, _)| std::cmp::Reverse(r.start));
                        for (range, replacement) in descendants {
                            let start = atom.offset + range.start - atom.source.start;
                            let end = atom.offset + range.end - atom.source.start;
                            xml.replace_range(start..end, replacement);
                        }
                    }
                    result.push_str(&xml);
                }
                _ => return Err(Error::Translation),
            }
            rest = &rest[end..];
        }
        self.base
            .write(&mut result, rest, !self.base.hyperlink, target_language)?;
        if seen.iter().zip(&self.tags).any(|(seen, tag)| {
            !seen && !(simplified && matches!(tag, Tag::Style(run) if !run.hyperlink))
        }) {
            return Err(Error::Translation);
        }
        if !self.after.is_empty() {
            result.push_str(&rewrite_properties(&self.after, Some(target_language))?);
        }
        Ok(result)
    }
}

impl Run {
    fn write(
        &self,
        out: &mut String,
        text: &str,
        allowed: bool,
        target_language: &str,
    ) -> Result<(), Error> {
        if text.is_empty() {
            return Ok(());
        }
        if !allowed
            || text
                .chars()
                .any(|c| c.is_control() && !matches!(c, '\t' | '\r' | '\n'))
        {
            return Err(Error::Translation);
        }
        if !self.word
            && let Some(props) = &self.props
        {
            out.push_str(&self.write_before[..props.start]);
            let properties = &self.write_before[props.clone()];
            // A private-use symbol is mapped by its original symbol font and
            // script. The target proofing language can select a different face.
            if text.chars().all(|c| ('\u{e000}'..='\u{f8ff}').contains(&c)) {
                out.push_str(properties);
            } else {
                out.push_str(&rewrite_properties(properties, Some(target_language))?);
            }
            out.push_str(&self.write_before[props.end..]);
        } else {
            out.push_str(&self.write_before);
        }
        out.push_str(&escape(text));
        out.push_str(&self.write_after);
        Ok(())
    }
}

/// Only the opening element is adjusted. Nested formatting and relationship IDs stay intact.
fn rewrite_properties(xml: &str, target: Option<&str>) -> Result<String, Error> {
    let mut reader = quick_xml::Reader::from_str(xml);
    let event = reader.read_event().map_err(|_| Error::Xml)?;
    let (element, empty) = match event {
        Event::Start(e) => (e.clone(), false),
        Event::Empty(e) => (e.clone(), true),
        _ => return Err(Error::Xml),
    };
    let attributes = element
        .attributes()
        .collect::<Result<Vec<_>, _>>()
        .map_err(|_| Error::Xml)?;
    let mut rebuilt = element.clone().into_owned();
    rebuilt.clear_attributes();
    for attr in attributes {
        let key = attr.key.as_ref();
        match target {
            None if matches!(key, b"lang" | b"altLang" | b"dirty" | b"err" | b"smtClean") => {
                continue;
            }
            Some(_) if key == b"err" => continue,
            Some(lang) if key == b"lang" && !lang.is_empty() => {
                let lang = match lang {
                    "en" => "en-US",
                    "ja" => "ja-JP",
                    _ => lang,
                };
                rebuilt.push_attribute(("lang", lang));
            }
            _ => rebuilt.push_attribute(attr),
        }
    }
    let fragment = if empty {
        Event::Empty(rebuilt)
    } else {
        Event::Start(rebuilt)
    };
    let mut writer = quick_xml::Writer::new(Vec::new());
    writer.write_event(fragment)?;
    let mut out = String::from_utf8(writer.into_inner()).map_err(|_| Error::Xml)?;
    out.push_str(&xml[reader.buffer_position() as usize..]);
    Ok(out)
}

struct ParsedTag<'a> {
    start: usize,
    id: usize,
    body: Option<&'a str>,
    body_start: usize,
    end: usize,
}

impl TagFormat {
    fn wrap(self, id: usize, text: &str, atom: bool) -> String {
        match self {
            Self::Anchor => format!("<a i=\"{id}\">{}</a>", if atom { "◇" } else { text }),
        }
    }

    fn atom_body(self) -> Option<&'static str> {
        match self {
            Self::Anchor => Some("◇"),
        }
    }

    fn next(self, text: &str) -> Result<Option<ParsedTag<'_>>, Error> {
        let prefixes: &[&str] = match self {
            Self::Anchor => &["<a i=", "</a>"],
        };
        let Some((start, prefix)) = prefixes
            .iter()
            .filter_map(|&p| text.find(p).map(|i| (i, p)))
            .min()
        else {
            return Ok(None);
        };
        let rest = &text[start + prefix.len()..];
        let (id, body, body_start, end) = match prefix {
            "<a i=" => {
                let (raw_id, content) = rest.split_once('>').ok_or(Error::Translation)?;
                let id = raw_id
                    .strip_prefix('"')
                    .and_then(|s| s.strip_suffix('"'))
                    .ok_or(Error::Translation)?;
                let close = "</a>";
                let (body, _) = content.split_once(close).ok_or(Error::Translation)?;
                if prefixes.iter().any(|p| body.contains(p)) {
                    return Err(Error::Translation);
                }
                (
                    id,
                    Some(body),
                    text.len() - content.len(),
                    text.len() - content.len() + body.len() + close.len(),
                )
            }
            _ => return Err(Error::Translation),
        };
        let id = id
            .parse::<usize>()
            .ok()
            .and_then(|n| n.checked_sub(1))
            .ok_or(Error::Translation)?;
        Ok(Some(ParsedTag {
            start,
            id,
            body,
            body_start,
            end,
        }))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn word_layout_risks_use_outer_paragraph_positions() {
        let xml = r#"<w:document xmlns:w="http://schemas.openxmlformats.org/wordprocessingml/2006/main" xmlns:wp="http://schemas.openxmlformats.org/drawingml/2006/wordprocessingDrawing"><w:body><w:p><w:r><w:drawing><wp:anchor><w:txbxContent><w:p><w:r><w:t>Box</w:t></w:r></w:p></w:txbxContent></wp:anchor></w:drawing></w:r></w:p><w:p><w:r><w:t>Label</w:t></w:r></w:p><w:tbl><w:tblPr><w:tblpPr/></w:tblPr><w:tr><w:trPr><w:trHeight w:hRule="exact"/></w:trPr><w:tc><w:p><w:r><w:t>Cell</w:t></w:r></w:p></w:tc></w:tr></w:tbl></w:body></w:document>"#;
        let part = Part {
            name: "word/document.xml".into(),
            archive_index: 0,
            xml: xml.into(),
            paragraphs: Vec::new(),
            frames: Vec::new(),
            word_regions: Vec::new(),
            symbol_theme: HashMap::new(),
            drawing_cache: None,
        };
        assert_eq!(part.word_layout_risks(), vec![1, 3]);
        assert_eq!(part.xml, xml);
    }

    #[test]
    fn reflows_the_tax_deck_sentences_within_their_own_boxes() {
        let root = r#"<root xmlns:a="http://schemas.openxmlformats.org/drawingml/2006/main">"#;
        let paragraph = |text, after| {
            format!(
                r#"<a:p><a:pPr><a:spcAft><a:spcPts val="{after}"/></a:spcAft><a:buNone/></a:pPr><a:r><a:rPr sz="1200"/><a:t>{text}</a:t></a:r></a:p>"#
            )
        };
        let xml = format!(
            "{root}<txBody>{}{}{}{}</txBody><txBody>{}{}</txBody></root>",
            paragraph("お店で商品を買いました。", 0),
            paragraph("代金 1,000円と、", 0),
            paragraph("税金100円を", 400),
            paragraph("支払いました。", 0),
            paragraph("街を清潔に", 0),
            paragraph("保つために 4.5%", 0),
        );
        let (groups, skipped) = parse_paragraphs(&xml, true).unwrap();
        assert_eq!(skipped, 0);
        let texts: Vec<_> = groups.iter().map(Paragraph::plain_text).collect();
        assert_eq!(
            texts,
            [
                "お店で商品を買いました。",
                "代金 1,000円と、税金100円を支払いました。",
                "街を清潔に保つために 4.5%",
            ]
        );

        let mut edited = xml.clone();
        let paid = "I paid 1,000 yen for the item and 100 yen in tax.";
        edited.replace_range(
            groups[1].body.clone(),
            &groups[1].rebuild(TagFormat::Anchor, paid, "en").unwrap(),
        );
        let (written, skipped) = paragraphs(&edited).unwrap();
        assert_eq!(skipped, 0);
        assert_eq!(written[1].plain_text(), paid);
        assert_eq!(written.len(), 4);
        assert_eq!(paragraphs(&xml).unwrap().0.len(), 6);
    }

    #[test]
    fn warns_about_overflow_and_preserves_small_reading_guides() {
        let xml = concat!(
            r#"<root xmlns:a="http://schemas.openxmlformats.org/drawingml/2006/main">"#,
            r#"<sp><spPr><a:xfrm><a:ext cx="762000" cy="190500"/></a:xfrm></spPr>"#,
            r#"<txBody><a:bodyPr wrap="none" tIns="0" bIns="0"/>"#,
            r#"<a:p><a:r><a:rPr sz="700"/><a:t>ぜいむしょ</a:t></a:r></a:p></txBody></sp>"#,
            r#"<sp><spPr><a:xfrm><a:ext cx="762000" cy="254000"/></a:xfrm></spPr>"#,
            r#"<txBody><a:bodyPr wrap="none" tIns="0" bIns="0"/>"#,
            r#"<a:p><a:r><a:rPr sz="1200"/><a:t>消費税</a:t></a:r></a:p></txBody></sp></root>"#,
        );
        let (paragraphs, _) = parse_paragraphs(xml, true).unwrap();
        let frames = layout::frames(xml).unwrap();
        assert!(frames[0].contains(&paragraphs[0]));
        assert!(!frames[0].contains(&paragraphs[1]));
        assert!(frames[0].likely_furigana(&paragraphs[0]));
        assert!(!frames[1].likely_furigana(&paragraphs[1]));
        assert!(
            frames[1]
                .fit(
                    std::iter::once((
                        &paragraphs[1],
                        Some(&"Consumption tax collected by the store".into())
                    )),
                    TagFormat::Anchor,
                )
                .is_some()
        );
        assert!(
            frames[1]
                .fit(
                    std::iter::once((&paragraphs[1], Some(&"Tax".into()))),
                    TagFormat::Anchor,
                )
                .is_none()
        );
        assert!(has_untranslated_text(r#"<chart><v>消費税</v></chart>"#).unwrap());
        assert!(!has_untranslated_text(r#"<chart><v>4.5%</v></chart>"#).unwrap());
    }

    #[test]
    fn yen_amounts_are_not_split_across_style_tags_and_keep_their_value() {
        let xml = concat!(
            r#"<root xmlns:a="http://schemas.openxmlformats.org/drawingml/2006/main"><a:p>"#,
            r#"<a:r><a:rPr sz="900"/><a:t>約</a:t></a:r>"#,
            r#"<a:r><a:rPr sz="2000"/><a:t>1兆6,721</a:t></a:r>"#,
            r#"<a:r><a:rPr sz="900"/><a:t>億円を計上</a:t></a:r></a:p></root>"#,
        );
        let (parts, skipped) = parse_paragraphs(xml, true).unwrap();
        assert_eq!(skipped, 0);
        let p = &parts[0];
        assert!(p.source(TagFormat::Anchor).contains("1兆6,721億円"));
        assert!(p.amounts_match(TagFormat::Anchor, "About 1.6721 trillion yen", "en"));
        assert!(p.amounts_match(TagFormat::Anchor, "About ¥1,672,100,000,000", "en"));
        assert!(!p.amounts_match(TagFormat::Anchor, "About 1.6721 billion yen", "en"));
        assert!(amounts::matches(
            "115兆1,978億円",
            "115 trillion 197.8 billion yen",
            "en"
        ));
        assert!(amounts::matches(
            "約2兆6,002億円",
            "2 trillion and 600.2 billion yen",
            "en"
        ));
        assert!(!p.amounts_match(TagFormat::Anchor, "About 1.6721 trillion", "en"));
        assert!(amounts::matches(
            "about 123 million yen",
            "約１億２，３００万円",
            "ja"
        ));
        assert!(!amounts::matches("123 million yen", "１２３億円", "ja"));
        assert!(amounts::matches(
            "Sales may rise 123 points.",
            "売上は123ポイント伸びるかもしれない。",
            "ja"
        ));
    }

    #[test]
    fn word_preserves_textless_drawings_beside_translated_instructions() {
        let prefix = r#"<root xmlns:w="http://schemas.openxmlformats.org/wordprocessingml/2006/main" xmlns:mc="http://schemas.openxmlformats.org/markup-compatibility/2006">"#;
        let drawing = r#"<mc:AlternateContent><mc:Choice Requires="wps"><w:drawing><shape/><offset>1050925</offset></w:drawing></mc:Choice><mc:Fallback><w:pict><shape/></w:pict></mc:Fallback></mc:AlternateContent>"#;
        for mixed in [false, true] {
            let middle = if mixed { "" } else { "</w:r><w:r>" };
            let xml = format!(
                "{prefix}<w:p><w:r>{drawing}{middle}<w:t>太枠内に○を記入してください。</w:t></w:r></w:p></root>"
            );
            let (paragraphs, skipped) = parse_office_paragraphs(&xml, true, Format::Word).unwrap();
            assert_eq!(skipped, 0);
            assert_eq!(paragraphs.len(), 1);
            let source = paragraphs[0].source(TagFormat::Anchor);
            let target = source.replace(
                "太枠内に○を記入してください。",
                "Place a circle inside the bold border.",
            );
            let rebuilt = paragraphs[0]
                .rebuild(TagFormat::Anchor, &target, "en")
                .unwrap();
            assert!(rebuilt.contains(drawing));
            assert!(rebuilt.contains("Place a circle"));
            assert!(!rebuilt.contains("太枠"));
        }
        let xml = format!(
            "{prefix}<w:p><w:r><w:drawing><w:txbxContent><w:p><w:r><w:t>図形の文字</w:t></w:r></w:p></w:txbxContent></w:drawing></w:r><w:r><w:t>本文</w:t></w:r></w:p></root>"
        );
        let (paragraphs, skipped) = parse_office_paragraphs(&xml, true, Format::Word).unwrap();
        assert_eq!(paragraphs.len(), 2);
        assert_eq!(skipped, 0);
        assert_eq!(paragraphs[0].plain_text().trim(), "本文");
        assert_eq!(paragraphs[1].plain_text(), "図形の文字");
        assert_eq!(paragraphs[0].word_location(), Some((1, false)));
        assert_eq!(paragraphs[1].word_location(), Some((1, false)));
    }

    fn word_package(xml: &str) -> Vec<u8> {
        let mut zip = ZipWriter::new(Cursor::new(Vec::new()));
        for (name, contents) in [
            ("[Content_Types].xml", "<Types/>"),
            ("word/document.xml", xml),
            ("word/_rels/document.xml.rels", "<Relationships/>"),
            ("word/media/untouched.bin", "original asset"),
        ] {
            zip.start_file(name, SimpleFileOptions::default()).unwrap();
            zip.write_all(contents.as_bytes()).unwrap();
        }
        zip.finish().unwrap().into_inner()
    }

    #[test]
    fn growing_word_table_uses_wrapping_boundary_and_keeps_nested_edits() {
        let xml = r#"<w:document xmlns:w="http://schemas.openxmlformats.org/wordprocessingml/2006/main" xmlns:wp="http://schemas.openxmlformats.org/drawingml/2006/wordprocessingDrawing" xmlns:wps="http://schemas.microsoft.com/office/word/2010/wordprocessingShape" xmlns:a="http://schemas.openxmlformats.org/drawingml/2006/main"><w:body><w:p><w:r><w:drawing><wp:anchor><wp:wrapSquare/><wps:wsp><wps:txbx><w:txbxContent><w:tbl><w:tr><w:tc><w:p><w:r><w:t>Cell</w:t></w:r></w:p></w:tc></w:tr></w:tbl></w:txbxContent></wps:txbx><wps:bodyPr><a:noAutofit/></wps:bodyPr></wps:wsp></wp:anchor></w:drawing></w:r><w:r><w:t>Body</w:t></w:r></w:p></w:body></w:document>"#;
        for wraps in [true, false] {
            let xml = if wraps {
                xml.into()
            } else {
                xml.replace("wrapSquare", "wrapNone")
            };
            let doc = OfficeDocument::open_format(word_package(&xml), true, Format::Word).unwrap();
            let format = TagFormat::Anchor;
            let src = doc.parts()[0].paragraphs[0].source(format);
            let targets = [vec![
                Some(src.replace("Body", "本文").into()),
                Some("表の中の新しい訳".into()),
            ]];
            assert_eq!(
                doc.word_overflow_reviews(format, &targets),
                [("word/document.xml".into(), 1)]
            );
            let out = OfficeDocument::open_format(
                doc.write(format, "ja", &targets).unwrap(),
                false,
                Format::Word,
            )
            .unwrap();
            assert_eq!(out.parts()[0].xml.contains("<a:spAutoFit/>"), wraps);
            assert!(out.parts()[0].xml.contains("表の中の新しい訳"));
            assert!(out.parts()[0].xml.contains("本文"));
            assert!(
                doc.word_overflow_reviews(format, &[vec![None, None]])
                    .is_empty()
            );
        }
    }

    #[test]
    fn compatibility_table_growth_requires_one_matching_vml_textbox() {
        let xml = r#"<w:document xmlns:w="http://schemas.openxmlformats.org/wordprocessingml/2006/main" xmlns:wp="http://schemas.openxmlformats.org/drawingml/2006/wordprocessingDrawing" xmlns:wps="http://schemas.microsoft.com/office/word/2010/wordprocessingShape" xmlns:a="http://schemas.openxmlformats.org/drawingml/2006/main" xmlns:mc="http://schemas.openxmlformats.org/markup-compatibility/2006" xmlns:v="urn:schemas-microsoft-com:vml"><w:body><w:p><w:r><mc:AlternateContent><mc:Choice Requires="wps"><w:drawing><wp:anchor><wp:wrapSquare/><wps:wsp><wps:txbx><w:txbxContent><w:tbl><w:tr><w:tc><w:p><w:r><w:t>Cell</w:t></w:r></w:p></w:tc></w:tr></w:tbl></w:txbxContent></wps:txbx><wps:bodyPr><a:noAutofit/></wps:bodyPr></wps:wsp></wp:anchor></w:drawing></mc:Choice><mc:Fallback><w:pict>FALLBACK</w:pict></mc:Fallback></mc:AlternateContent></w:r><w:r><w:t>Outer</w:t></w:r></w:p></w:body></w:document>"#;
        let shape = r#"<v:shape><v:textbox inset="0" style="color:red;mso-fit-shape-to-text:f"><w:txbxContent><w:tbl><w:tr><w:tc><w:p><w:r><w:t>Cell</w:t></w:r></w:p></w:tc></w:tr></w:tbl></w:txbxContent></v:textbox></v:shape>"#;
        for (fallback, paired) in [
            (shape.into(), true),
            (shape.repeat(2), false),
            ("<v:shape/>".into(), false),
        ] {
            let doc = OfficeDocument::open_format(
                word_package(&xml.replace("FALLBACK", &fallback)),
                true,
                Format::Word,
            )
            .unwrap();
            let targets = [vec![
                Some(
                    doc.parts()[0].paragraphs[0]
                        .source(TagFormat::Anchor)
                        .replace("Outer", "本文")
                        .into(),
                ),
                Some("表内".into()),
            ]];
            let bytes = doc.write(TagFormat::Anchor, "ja", &targets).unwrap();
            let out = OfficeDocument::open_format(bytes, false, Format::Word).unwrap();
            let text = &out.parts()[0].xml;
            assert_eq!(text.contains("<a:spAutoFit/>"), paired);
            assert_eq!(text.contains("color:red;mso-fit-shape-to-text:t"), paired);
            assert!(text.contains("表内") && text.contains("本文"));
        }
    }

    #[test]
    fn unmeasured_placeholders_and_notes_keep_their_translation() {
        let xml = r#"<root xmlns:a="http://schemas.openxmlformats.org/drawingml/2006/main"><sp><ph type="body"/><txBody><a:bodyPr/><a:p><a:r><a:rPr sz="1400"/><a:t>Reading room</a:t></a:r></a:p></txBody></sp></root>"#;
        let mut zip = ZipWriter::new(Cursor::new(Vec::new()));
        for (name, contents) in [
            ("[Content_Types].xml", "<Types/>"),
            ("ppt/presentation.xml", "<presentation/>"),
            ("ppt/slides/slide1.xml", xml),
            ("ppt/notesSlides/notesSlide1.xml", xml),
        ] {
            zip.start_file(name, SimpleFileOptions::default()).unwrap();
            zip.write_all(contents.as_bytes()).unwrap();
        }
        let doc = OfficeDocument::open_mode(zip.finish().unwrap().into_inner(), true).unwrap();
        let targets = vec![vec![Some("読書室の利用案内".into())]; 2];
        assert!(
            doc.powerpoint_layout_reviews(TagFormat::Anchor, &targets)
                .is_empty()
        );
        let out =
            OfficeDocument::open(doc.write(TagFormat::Anchor, "ja", &targets).unwrap()).unwrap();
        for part in out.parts() {
            assert_eq!(part.paragraphs[0].plain_text(), "読書室の利用案内");
        }
    }

    #[test]
    fn word_writes_inner_and_outer_translations_without_overlapping_edits() {
        for mixed_run in [false, true] {
            for alternate in [false, true] {
                let textbox = r#"<w:drawing><w:txbxContent><w:p><w:r><w:t>Box</w:t></w:r></w:p><w:p><w:r><w:t>Box</w:t></w:r></w:p></w:txbxContent></w:drawing>"#;
                let fallback = r#"<mc:Fallback><w:pict><w:txbxContent><w:p><w:r><w:t>Box fallback</w:t></w:r></w:p></w:txbxContent></w:pict></mc:Fallback>"#;
                let drawing = if alternate {
                    format!(
                        r#"<mc:AlternateContent><mc:Choice Requires="wps">{textbox}</mc:Choice>{fallback}</mc:AlternateContent>"#
                    )
                } else {
                    textbox.to_owned()
                };
                let middle = if mixed_run { "" } else { "</w:r><w:r>" };
                let xml = format!(
                    r#"<w:document xmlns:w="http://schemas.openxmlformats.org/wordprocessingml/2006/main" xmlns:mc="http://schemas.openxmlformats.org/markup-compatibility/2006"><w:body><w:p><w:r>{drawing}{middle}<w:t>Body</w:t></w:r></w:p><w:p><w:r><w:t>Following</w:t></w:r></w:p></w:body></w:document>"#
                );
                let original = word_package(&xml);
                let document =
                    OfficeDocument::open_format(original.clone(), true, Format::Word).unwrap();
                assert!(document.unsupported.is_empty());
                let paragraphs = &document.parts[0].paragraphs;
                assert_eq!(paragraphs.len(), 4);
                assert_eq!(paragraphs[3].word_location(), Some((2, false)));
                let translated = ["本文", "枠の一段落目", "枠の二段落目は別の訳", "次の段落"];
                // Cover independent failures/retentions of the parent and each child.
                for mask in 0..16 {
                    let targets = paragraphs
                        .iter()
                        .enumerate()
                        .map(|(i, p)| {
                            (mask & (1 << i) != 0).then(|| {
                                p.source(TagFormat::Anchor)
                                    .replace(p.plain_text().trim(), translated[i])
                                    .into()
                            })
                        })
                        .collect::<Vec<_>>();
                    let output = document.write(TagFormat::Anchor, "ja", &[targets]).unwrap();
                    if mask == 0 {
                        assert_eq!(output, original);
                    }
                    let reopened =
                        OfficeDocument::open_format(output.clone(), true, Format::Word).unwrap();
                    assert!(reopened.unsupported.is_empty());
                    assert_eq!(reopened.parts[0].paragraphs.len(), 4);
                    for (i, p) in reopened.parts[0].paragraphs.iter().enumerate() {
                        let original_text = paragraphs[i].plain_text();
                        assert_eq!(
                            p.plain_text().trim(),
                            if mask & (1 << i) != 0 {
                                translated[i]
                            } else {
                                original_text.trim()
                            }
                        );
                    }
                    if alternate {
                        assert!(reopened.parts[0].xml.contains(fallback));
                    }
                    let mut before = ZipArchive::new(Cursor::new(&original)).unwrap();
                    let mut after = ZipArchive::new(Cursor::new(&output)).unwrap();
                    for name in [
                        "[Content_Types].xml",
                        "word/_rels/document.xml.rels",
                        "word/media/untouched.bin",
                    ] {
                        let mut a = Vec::new();
                        let mut b = Vec::new();
                        before.by_name(name).unwrap().read_to_end(&mut a).unwrap();
                        after.by_name(name).unwrap().read_to_end(&mut b).unwrap();
                        assert_eq!(a, b);
                    }
                }
            }
        }
    }

    #[test]
    fn word_reports_protected_textbox_contents_without_locking_surrounding_body() {
        for protected in [
            "<w:ins><w:p><w:r><w:t>Protected</w:t></w:r></w:p></w:ins>",
            "<w:p><w:ins><w:r><w:t>Protected</w:t></w:r></w:ins></w:p>",
            "<w:sdt><w:sdtPr><w:dataBinding/></w:sdtPr><w:sdtContent><w:p><w:r><w:t>Protected</w:t></w:r></w:p></w:sdtContent></w:sdt>",
        ] {
            let xml = format!(
                r#"<w:document xmlns:w="http://schemas.openxmlformats.org/wordprocessingml/2006/main"><w:body><w:p><w:r><w:drawing><w:txbxContent>{protected}<w:p><w:r><w:t>Editable</w:t></w:r></w:p></w:txbxContent></w:drawing></w:r><w:r><w:t>Body</w:t></w:r></w:p></w:body></w:document>"#
            );
            let document =
                OfficeDocument::open_format(word_package(&xml), true, Format::Word).unwrap();
            assert_eq!(document.unsupported.len(), 1);
            assert_eq!(document.unsupported[0].source, "Protected");
            assert_eq!(document.unsupported[0].paragraph, Some(1));
            let paragraphs = &document.parts[0].paragraphs;
            assert_eq!(paragraphs.len(), 2);
            let targets = paragraphs
                .iter()
                .map(|p| {
                    Some(
                        p.source(TagFormat::Anchor)
                            .replace("Body", "本文")
                            .replace("Editable", "編集可能")
                            .into(),
                    )
                })
                .collect();
            let output = document.write(TagFormat::Anchor, "ja", &[targets]).unwrap();
            let reopened = OfficeDocument::open_format(output, true, Format::Word).unwrap();
            assert!(reopened.parts[0].xml.contains(protected));
            assert_eq!(reopened.unsupported.len(), 1);
            assert_eq!(reopened.parts[0].paragraphs[0].plain_text().trim(), "本文");
            assert_eq!(reopened.parts[0].paragraphs[1].plain_text(), "編集可能");
        }
    }

    #[test]
    fn word_nested_textboxes_keep_bookmarks_and_each_translation() {
        let xml = r#"<w:document xmlns:w="http://schemas.openxmlformats.org/wordprocessingml/2006/main" xmlns:mc="http://schemas.openxmlformats.org/markup-compatibility/2006"><w:body><w:p><w:r><w:t>Outer</w:t></w:r><mc:AlternateContent><mc:Choice Requires="wps"><w:drawing><w:txbxContent><w:p><w:bookmarkStart w:id="12"/><w:r><w:t>Middle</w:t></w:r><w:r><w:pict><w:txbxContent><w:tbl><w:tr><w:tc><w:p><w:r><w:t>Inner</w:t></w:r></w:p></w:tc></w:tr></w:tbl></w:txbxContent></w:pict></w:r><w:bookmarkEnd w:id="12"/></w:p></w:txbxContent></w:drawing></mc:Choice><mc:Fallback/></mc:AlternateContent></w:p></w:body></w:document>"#;
        let document = OfficeDocument::open_format(word_package(xml), true, Format::Word).unwrap();
        assert!(document.unsupported.is_empty());
        let paragraphs = &document.parts[0].paragraphs;
        assert_eq!(paragraphs.len(), 3);
        assert_eq!(paragraphs[2].word_location(), Some((1, true)));
        let replacements = [("Outer", "外側"), ("Middle", "中間"), ("Inner", "内側")];
        for mask in 0..8 {
            let targets = paragraphs
                .iter()
                .enumerate()
                .map(|(i, p)| {
                    (mask & (1 << i) != 0).then(|| {
                        p.source(TagFormat::Anchor)
                            .replace(replacements[i].0, replacements[i].1)
                            .into()
                    })
                })
                .collect();
            let output = document.write(TagFormat::Anchor, "ja", &[targets]).unwrap();
            let reopened = OfficeDocument::open_format(output, true, Format::Word).unwrap();
            assert!(reopened.unsupported.is_empty());
            assert_eq!(reopened.parts[0].paragraphs.len(), 3);
            for (i, p) in reopened.parts[0].paragraphs.iter().enumerate() {
                assert_eq!(
                    p.plain_text().trim(),
                    if mask & (1 << i) != 0 {
                        replacements[i].1
                    } else {
                        replacements[i].0
                    }
                );
            }
            for marker in [
                r#"<w:bookmarkStart w:id="12"/>"#,
                r#"<w:bookmarkEnd w:id="12"/>"#,
            ] {
                assert_eq!(reopened.parts[0].xml.matches(marker).count(), 1);
            }
        }
    }

    #[test]
    fn word_repairs_one_missing_close_between_ordinary_styles() {
        let xml = r#"<root xmlns:w="http://schemas.openxmlformats.org/wordprocessingml/2006/main"><w:p><w:r><w:rPr><w:b/></w:rPr><w:t>それぞれに</w:t></w:r><w:r><w:t>当てはまる項目の太枠内に○を記入</w:t></w:r><w:r><w:rPr><w:i/></w:rPr><w:t>してください。</w:t></w:r></w:p></root>"#;
        let (paragraphs, _) = parse_office_paragraphs(xml, true, Format::Word).unwrap();
        let p = &paragraphs[0];
        let target = r#"<a i="1">Please mark a circle inside the bold box for each applicable item<a i="2">.</a>"#;
        let rebuilt = p.rebuild(TagFormat::Anchor, target, "en").unwrap();
        assert!(rebuilt.contains("Please mark a circle"));
        assert!(!p.valid(TagFormat::Anchor, &target.replace("i=\"2\"", "i=\"1\"")));
        assert!(!p.valid(TagFormat::Anchor, &target.replace("i=\"2\"", "i=\"9\"")));
        assert!(!p.valid(TagFormat::Anchor, r#"<a i="1">Missing second style</a>"#));
        let link = xml
            .replace("<w:r><w:rPr><w:b/>", "<w:hyperlink><w:r><w:rPr><w:b/>")
            .replace(
                "それぞれに</w:t></w:r>",
                "それぞれに</w:t></w:r></w:hyperlink>",
            );
        let (paragraphs, _) = parse_office_paragraphs(&link, true, Format::Word).unwrap();
        assert!(!paragraphs[0].valid(TagFormat::Anchor, target));
    }

    #[test]
    fn word_restores_missing_english_spaces_at_style_boundaries() {
        let xml = concat!(
            r#"<root xmlns:w="http://schemas.openxmlformats.org/wordprocessingml/2006/main">"#,
            r#"<w:p><w:r><w:rPr><w:b/></w:rPr><w:t>粗利益率は</w:t></w:r>"#,
            r#"<w:r><w:rPr><w:i/></w:rPr><w:t xml:space="default">低下</w:t></w:r>"#,
            r#"<w:r><w:t>し、38.5%となりました。</w:t></w:r></w:p>"#,
            r#"<w:p><w:r><w:t>Open </w:t></w:r><w:r><w:rPr><w:b/></w:rPr><w:t>Power</w:t></w:r><w:r><w:t>Point now.</w:t></w:r></w:p>"#,
            r#"<w:p><w:r><w:t>価格は</w:t></w:r><w:r><w:rPr><w:b/></w:rPr><w:t>１億円</w:t></w:r>"#,
            r#"<w:r><w:t>です。</w:t></w:r></w:p></root>"#,
        );
        let (paragraphs, skipped) = parse_office_paragraphs(xml, true, Format::Word).unwrap();
        assert_eq!(skipped, 0);
        let format = TagFormat::Anchor;
        let p = &paragraphs[0];
        let spacing = r#"<a i="1">The gross profit margin</a><a i="2">fell</a>to 38.5%."#;
        let rebuilt = p.rebuild(format, spacing, "en").unwrap();
        assert!(rebuilt.contains(r#"<w:rPr><w:i/></w:rPr><w:t xml:space="preserve"> fell</w:t>"#));
        assert!(rebuilt.contains(r#"<w:t xml:space="preserve"> to 38.5%.</w:t>"#));
        assert!(!rebuilt.contains(r#"xml:space="default""#));
        let natural = r#"<a i="1">The gross profit margin</a> <a i="2">fell</a>, to 38.5%."#;
        assert_eq!(p.with_word_spaces(format, natural, "en").unwrap(), natural);
        assert_eq!(p.with_word_spaces(format, spacing, "fr").unwrap(), spacing);

        let p = &paragraphs[1];
        let brand = r#"Open<a i="1">Power</a>Point now."#;
        assert_eq!(
            p.with_word_spaces(format, brand, "en").unwrap(),
            r#"Open<a i="1"> Power</a>Point now."#
        );
        assert!(paragraphs[2].amounts_match(
            format,
            r#"The price is <a i="1">100 million</a>yen."#,
            "en"
        ));
    }

    #[test]
    fn word_inline_text_controls_preserve_nested_wrappers_and_order() {
        for properties in [
            "",
            "<w:sdtPr/>",
            r#"<w:sdtPr><w:id w:val="42"/><w:alias w:val="Label"/><w:text/></w:sdtPr>"#,
        ] {
            let xml = format!(
                r#"<w:document xmlns:w="http://schemas.openxmlformats.org/wordprocessingml/2006/main"><w:body><w:p><w:r><w:t>Before</w:t></w:r><w:sdt>{properties}<w:sdtContent><w:r><w:t>Inside</w:t></w:r><w:sdt><w:sdtPr><w:id w:val="43"/></w:sdtPr><w:sdtContent><w:r><w:rPr><w:b/></w:rPr><w:t>Nested</w:t></w:r></w:sdtContent></w:sdt></w:sdtContent></w:sdt><w:r><w:t>After</w:t></w:r></w:p></w:body></w:document>"#
            );
            let document =
                OfficeDocument::open_format(word_package(&xml), true, Format::Word).unwrap();
            assert!(document.unsupported.is_empty());
            assert_eq!(document.parts[0].paragraphs.len(), 1);
            let p = &document.parts[0].paragraphs[0];
            let source = p.source(TagFormat::Anchor);
            let target = source
                .replace("Before", "前")
                .replace("Inside", "内部")
                .replace("Nested", "入れ子")
                .replace("After", "後");
            let swapped = target
                .replace("i=\"1\"", "i=\"TMP\"")
                .replace("i=\"2\"", "i=\"1\"")
                .replace("i=\"TMP\"", "i=\"2\"");
            assert!(!p.valid(TagFormat::Anchor, &swapped));
            assert!(!p.valid(TagFormat::Anchor, &target.replace(r#"<a i="1">◇</a>"#, "")));
            let output = document
                .write(TagFormat::Anchor, "ja", &[vec![Some(target.into())]])
                .unwrap();
            let reopened = OfficeDocument::open_format(output, true, Format::Word).unwrap();
            assert!(reopened.unsupported.is_empty());
            let part = &reopened.parts[0];
            assert_eq!(part.xml.matches("<w:sdt>").count(), 2);
            assert!(part.xml.contains(properties));
            assert_eq!(part.xml.matches(r#"<w:id w:val="43"/>"#).count(), 1);
            assert!(part.xml.contains("<w:b/>"));
            assert_eq!(
                part.paragraphs[0]
                    .plain_text()
                    .split_whitespace()
                    .collect::<String>(),
                "前内部入れ子後"
            );
        }
    }

    #[test]
    fn word_controls_in_textboxes_and_multirun_links_keep_scoped_text() {
        let xml = r#"<w:document xmlns:w="http://schemas.openxmlformats.org/wordprocessingml/2006/main" xmlns:r="http://schemas.openxmlformats.org/officeDocument/2006/relationships"><w:body><w:p><w:r><w:drawing><w:txbxContent><w:sdt><w:sdtPr><w:id w:val="14"/></w:sdtPr><w:sdtContent><w:p><w:r><w:t>Box</w:t></w:r></w:p></w:sdtContent></w:sdt></w:txbxContent></w:drawing></w:r><w:sdt><w:sdtContent><w:r><w:tab/><w:t>Control</w:t></w:r></w:sdtContent></w:sdt><w:hyperlink r:id="rId7"><w:r><w:t>Link</w:t></w:r><w:r><w:rPr><w:b/></w:rPr><w:t>Label</w:t></w:r></w:hyperlink></w:p></w:body></w:document>"#;
        let document = OfficeDocument::open_format(word_package(xml), true, Format::Word).unwrap();
        assert!(document.unsupported.is_empty());
        assert_eq!(document.parts[0].paragraphs.len(), 2);
        let p = &document.parts[0].paragraphs[0];
        let source = p.source(TagFormat::Anchor);
        assert!(source.contains(">Control</a>"));
        assert!(!p.valid(
            TagFormat::Anchor,
            &source.replace(">Control</a>", "></a>Control")
        ));
        let targets = document.parts[0]
            .paragraphs
            .iter()
            .map(|p| {
                Some(
                    p.source(TagFormat::Anchor)
                        .replace("Box", "枠")
                        .replace("Control", "説明欄")
                        .replace("Link", "リンク")
                        .replace("Label", "表示")
                        .into(),
                )
            })
            .collect();
        let output = document.write(TagFormat::Anchor, "ja", &[targets]).unwrap();
        let reopened = OfficeDocument::open_format(output, true, Format::Word).unwrap();
        assert!(reopened.unsupported.is_empty());
        let part = &reopened.parts[0];
        assert_eq!(part.paragraphs[1].plain_text(), "枠");
        assert!(part.paragraphs[0].plain_text().contains("説明欄"));
        assert_eq!(part.xml.matches(r#"<w:hyperlink r:id="rId7">"#).count(), 1);
        assert_eq!(part.xml.matches(r#"<w:id w:val="14"/>"#).count(), 1);
        assert_eq!(part.xml.matches("<w:tab/>").count(), 1);
    }

    #[test]
    fn word_preserves_typed_controls_and_revisions_beside_editable_text() {
        let mut protected = vec![
            r#"<w:ins w:id="7"><w:r><w:t>Inserted proposal</w:t></w:r></w:ins>"#.to_owned(),
            r#"<w:del w:id="8"><w:r><w:delText>Deleted proposal</w:delText></w:r></w:del>"#
                .to_owned(),
        ];
        for prop in [
            "dataBinding",
            "dropDownList",
            "comboBox",
            "date",
            "checkBox",
            "showingPlcHdr",
            "lock",
        ] {
            protected.push(format!("<w:sdt><w:sdtPr><w:{prop}/></w:sdtPr><w:sdtContent><w:r><w:t>Protected value</w:t></w:r></w:sdtContent></w:sdt>"));
        }
        for original in protected {
            let xml = format!(
                r#"<w:document xmlns:w="http://schemas.openxmlformats.org/wordprocessingml/2006/main"><w:body><w:p><w:r><w:t>Before</w:t></w:r>{original}<w:r><w:t>After</w:t></w:r></w:p></w:body></w:document>"#
            );
            let document =
                OfficeDocument::open_format(word_package(&xml), true, Format::Word).unwrap();
            assert_eq!(document.unsupported.len(), 1);
            assert_eq!(document.unsupported[0].paragraph, Some(1));
            assert!(!document.unsupported[0].source.contains("Before"));
            let p = &document.parts[0].paragraphs[0];
            let target = p
                .source(TagFormat::Anchor)
                .replace("Before", "前")
                .replace("After", "後");
            let output = document
                .write(TagFormat::Anchor, "ja", &[vec![Some(target.into())]])
                .unwrap();
            let reopened = OfficeDocument::open_format(output, true, Format::Word).unwrap();
            assert!(reopened.parts[0].xml.contains(&original));
            assert_eq!(reopened.unsupported.len(), 1);
            assert_eq!(reopened.parts[0].paragraphs[0].plain_text().trim(), "前 後");
        }
    }

    #[test]
    fn word_translates_only_hyperlink_field_results() {
        let result =
            r#"<w:r><w:t>Link</w:t></w:r><w:r><w:rPr><w:b/></w:rPr><w:t>Label</w:t></w:r>"#;
        for simple in [false, true] {
            for command in ["HYPERLINK", "REF", "TOC"] {
                let (prefix, suffix) = if simple {
                    (
                        format!(r#"<w:fldSimple w:instr="{command} target">"#),
                        "</w:fldSimple>".to_owned(),
                    )
                } else {
                    (
                        format!(
                            r#"<w:r><w:fldChar w:fldCharType="begin"/></w:r><w:r><w:instrText>{command} </w:instrText></w:r><w:r><w:instrText>target</w:instrText></w:r><w:r><w:fldChar w:fldCharType="separate"/></w:r>"#
                        ),
                        r#"<w:r><w:fldChar w:fldCharType="end"/></w:r>"#.to_owned(),
                    )
                };
                let field = format!("{prefix}{result}{suffix}");
                let xml = format!(
                    r#"<w:document xmlns:w="http://schemas.openxmlformats.org/wordprocessingml/2006/main"><w:body><w:p><w:r><w:t>Before</w:t></w:r>{field}<w:r><w:t>After</w:t></w:r></w:p></w:body></w:document>"#
                );
                let document =
                    OfficeDocument::open_format(word_package(&xml), true, Format::Word).unwrap();
                assert_eq!(
                    document.unsupported.len(),
                    usize::from(command != "HYPERLINK")
                );
                if command == "TOC" {
                    assert_eq!(
                        document.unsupported[0].reason,
                        RetentionReason::TableOfContents
                    );
                }
                let p = &document.parts[0].paragraphs[0];
                let source = p.source(TagFormat::Anchor);
                assert_eq!(source.contains("Label"), command == "HYPERLINK");
                let target = source
                    .replace("Before", "前")
                    .replace("After", "後")
                    .replace("Link", "リンク")
                    .replace("Label", "見出し");
                let output = document
                    .write(TagFormat::Anchor, "ja", &[vec![Some(target.into())]])
                    .unwrap();
                let reopened = OfficeDocument::open_format(output, true, Format::Word).unwrap();
                let xml = &reopened.parts[0].xml;
                assert!(xml.contains(&prefix));
                assert!(xml.contains(&suffix));
                assert_eq!(xml.contains(&field), command != "HYPERLINK");
                assert_eq!(
                    reopened.parts[0].paragraphs[0]
                        .plain_text()
                        .contains("リンク"),
                    command == "HYPERLINK"
                );
            }
        }
    }

    #[test]
    fn word_preserves_cross_paragraph_fields_and_translates_their_surroundings() {
        let xml = r#"<w:document xmlns:w="http://schemas.openxmlformats.org/wordprocessingml/2006/main"><w:body><w:p><w:r><w:t>Introduction</w:t></w:r><w:r><w:fldChar w:fldCharType="begin"/></w:r><w:r><w:instrText>TOC</w:instrText></w:r><w:r><w:fldChar w:fldCharType="separate"/></w:r><w:r><w:t>First entry</w:t></w:r></w:p><w:p><w:r><w:t>Middle entry</w:t></w:r></w:p><w:p><w:r><w:t>Last entry</w:t></w:r><w:r><w:fldChar w:fldCharType="end"/></w:r><w:r><w:t>Conclusion</w:t></w:r></w:p></w:body></w:document>"#;
        let document = OfficeDocument::open_format(word_package(xml), true, Format::Word).unwrap();
        let paragraphs = &document.parts[0].paragraphs;
        assert_eq!(paragraphs.len(), 2);
        assert_eq!(document.unsupported.len(), 3);
        assert_eq!(
            document.unsupported[0].reason,
            RetentionReason::TableOfContents
        );
        assert_eq!(paragraphs[1].word_location(), Some((3, false)));
        let targets = paragraphs
            .iter()
            .map(|p| {
                Some(
                    p.source(TagFormat::Anchor)
                        .replace("Introduction", "導入")
                        .replace("Conclusion", "結論")
                        .into(),
                )
            })
            .collect();
        let output = document.write(TagFormat::Anchor, "ja", &[targets]).unwrap();
        let reopened = OfficeDocument::open_format(output, true, Format::Word).unwrap();
        assert_eq!(reopened.unsupported.len(), 3);
        for text in ["First entry", "Middle entry", "Last entry"] {
            assert_eq!(reopened.parts[0].xml.matches(text).count(), 1);
        }
        assert_eq!(reopened.parts[0].paragraphs[0].plain_text().trim(), "導入");
        assert_eq!(reopened.parts[0].paragraphs[1].plain_text().trim(), "結論");
        // With no end marker, everything after begin remains protected.
        let unclosed = xml.replace(r#"<w:r><w:fldChar w:fldCharType="end"/></w:r>"#, "");
        let document =
            OfficeDocument::open_format(word_package(&unclosed), true, Format::Word).unwrap();
        assert_eq!(document.parts[0].paragraphs.len(), 1);
        assert!(
            document
                .unsupported
                .iter()
                .any(|r| r.source.contains("Conclusion"))
        );
    }

    #[test]
    fn word_unbound_block_rich_text_keeps_control_properties_and_protects_typed_controls() {
        let wrap = |pr: &str| {
            format!(
                r#"<w:document xmlns:w="http://schemas.openxmlformats.org/wordprocessingml/2006/main"><w:body><w:sdt><w:sdtPr>{pr}</w:sdtPr><w:sdtContent><w:p><w:r><w:t>Policy objectives</w:t></w:r></w:p></w:sdtContent></w:sdt></w:body></w:document>"#
            )
        };
        let source = wrap(
            r#"<w:id w:val="42"/><w:placeholder><w:docPart w:val="original"/></w:placeholder>"#,
        );
        let (paragraphs, skipped) = parse_office_paragraphs(&source, true, Format::Word).unwrap();
        assert_eq!(skipped, 0);
        assert_eq!(paragraphs.len(), 1);
        let p = &paragraphs[0];
        let mut output = source.clone();
        output.replace_range(
            p.body.clone(),
            &p.rebuild(TagFormat::Anchor, "政策目標", "ja").unwrap(),
        );
        assert!(output.contains(
            r#"<w:id w:val="42"/><w:placeholder><w:docPart w:val="original"/></w:placeholder>"#
        ));
        for restricted in [
            "<w:dataBinding/>",
            "<w:dropDownList/>",
            "<w:showingPlcHdr/>",
            "<w:lock/>",
            "<w:date/>",
        ] {
            let (p, skipped) =
                parse_office_paragraphs(&wrap(restricted), true, Format::Word).unwrap();
            assert!(p.is_empty(), "{restricted}");
            assert_eq!(skipped, 1);
        }
    }

    #[test]
    fn word_preserves_links_and_simple_bookmarks_and_leaves_revisions_intact() {
        let xml = concat!(
            r#"<w:document xmlns:w="http://schemas.openxmlformats.org/wordprocessingml/2006/main" xmlns:r="http://schemas.openxmlformats.org/officeDocument/2006/relationships"><w:body>"#,
            r#"<w:p><w:pPr><w:pStyle w:val="Heading1"/></w:pPr><w:r><w:t>参照先：</w:t></w:r>"#,
            r#"<w:hyperlink r:id="rId5"><w:r><w:rPr><w:b/></w:rPr><w:t>対応表</w:t></w:r></w:hyperlink></w:p>"#,
            r#"<w:p><w:bookmarkStart w:id="1"/><w:r><w:t>保存</w:t></w:r><w:bookmarkEnd w:id="1"/></w:p>"#,
            r#"<w:p><w:ins w:id="2"><w:r><w:t>修正中</w:t></w:r></w:ins></w:p></w:body></w:document>"#,
        );
        let (paragraphs, skipped) = parse_office_paragraphs(xml, true, Format::Word).unwrap();
        assert_eq!(skipped, 1);
        assert_eq!(paragraphs.len(), 2);
        let p = &paragraphs[0];
        assert_eq!(
            p.source(TagFormat::Anchor),
            r#"参照先：<a i="1">対応表</a>"#
        );
        let rendered = p
            .rebuild(TagFormat::Anchor, r#"See <a i="1">mapping table</a>"#, "en")
            .unwrap();
        assert!(rendered.contains(r#"<w:hyperlink r:id="rId5">"#));
        assert!(rendered.contains(r#"<w:t xml:space="preserve">See </w:t>"#));
        assert!(rendered.contains("<w:rPr><w:b/></w:rPr>"));
        assert!(rendered.contains(r#"<w:pStyle w:val="Heading1"/>"#));
        let bookmark = paragraphs[1]
            .rebuild(TagFormat::Anchor, "Save", "en")
            .unwrap();
        assert!(bookmark.contains(r#"<w:bookmarkStart w:id="1"/><w:r><w:t xml:space="preserve">Save</w:t></w:r><w:bookmarkEnd w:id="1"/>"#));
        assert!(paragraphs[1].valid(TagFormat::Anchor, "Save"));
    }

    #[test]
    fn word_repairs_a_surplus_style_close_before_a_protected_atom() {
        let xml = format!(
            r#"<w:document xmlns:w="{}"><w:body><w:p>
            <w:r><w:t>通常の文</w:t></w:r><w:r><w:rPr><w:u w:val="single"/></w:rPr><w:t>内容</w:t></w:r>
            <w:r><w:br/></w:r><w:r><w:t>役割分担の確認</w:t></w:r>
            </w:p></w:body></w:document>"#,
            std::str::from_utf8(WORD).unwrap()
        );
        let (paragraphs, skipped) = parse_office_paragraphs(&xml, true, Format::Word).unwrap();
        assert_eq!(skipped, 0);
        let paragraph = &paragraphs[0];
        let output = r#"Did you confirm the <a i="1">nature</a>, and </a><a i="2">◇</a> the division of roles?"#;
        let format = TagFormat::Anchor;
        let rebuilt = paragraph.rebuild(format, output, "en").unwrap();
        assert!(rebuilt.contains(">nature</w:t>"));
        assert!(rebuilt.contains("<w:br/>"));
        assert!(rebuilt.contains("division of roles?"));
        assert!(!rebuilt.contains("&lt;/a&gt;"));
        assert!(paragraph.amounts_match(format, output, "en"));

        let missing_break = output.replace(r#"<a i="2">◇</a>"#, "");
        let wrong_break = output.replace("◇", "line");
        let duplicate_break = format!(r#"{output}<a i="2">◇</a>"#);
        assert!(!paragraph.valid(format, &missing_break));
        assert!(!paragraph.valid(format, &wrong_break));
        assert!(!paragraph.valid(format, &duplicate_break));
    }

    #[test]
    fn word_translates_text_sharing_a_run_with_tab_or_saved_page_break() {
        let xml = format!(
            r#"<w:document xmlns:w="{}"><w:body><w:tbl><w:tr><w:tc>
            <w:p><w:r><w:rPr><w:b/></w:rPr><w:lastRenderedPageBreak/><w:tab/><w:t>Business Needs</w:t></w:r></w:p>
            <w:p><w:sdt><w:sdtPr><w:dataBinding/></w:sdtPr><w:sdtContent><w:r><w:t>Protected form text</w:t></w:r></w:sdtContent></w:sdt></w:p>
            </w:tc></w:tr></w:tbl></w:body></w:document>"#,
            std::str::from_utf8(WORD).unwrap()
        );
        let (paragraphs, retained) = parse_paragraph_details(&xml, true, Format::Word).unwrap();
        assert_eq!(paragraphs.len(), 1);
        let heading = &paragraphs[0];
        assert_eq!(heading.word_location(), Some((1, true)));
        assert_eq!(
            heading.source(TagFormat::Anchor),
            r#"<a i="1">◇</a>Business Needs"#
        );
        let translated = heading
            .rebuild(TagFormat::Anchor, r#"<a i="1">◇</a>事業上の必要性"#, "ja")
            .unwrap();
        assert!(translated.contains("<w:tab/>"));
        assert!(translated.contains("事業上の必要性"));
        assert!(!translated.contains("lastRenderedPageBreak"));
        assert_eq!(retained[0].text, "Protected form text");
        assert!(retained[0].in_table);
    }

    #[test]
    fn word_translates_inside_balanced_ranges_and_preserves_endpoint_paragraphs() {
        let xml = concat!(
            r#"<w:document xmlns:w="http://schemas.openxmlformats.org/wordprocessingml/2006/main"><w:body>"#,
            r#"<w:p><w:commentRangeStart w:id="4"/><w:r><w:t>始点</w:t></w:r></w:p>"#,
            r#"<w:p><w:r><w:t>範囲内</w:t></w:r></w:p>"#,
            r#"<w:p><w:r><w:t>終点</w:t></w:r><w:commentRangeEnd w:id="4"/><w:r><w:commentReference w:id="4"/></w:r></w:p>"#,
            r#"<w:p><w:r><w:t>ページ </w:t></w:r><w:r><w:fldChar w:fldCharType="begin"/></w:r>"#,
            r#"<w:r><w:instrText xml:space="preserve"> PAGE </w:instrText></w:r>"#,
            r#"<w:r><w:fldChar w:fldCharType="separate"/></w:r><w:r><w:t>1</w:t></w:r>"#,
            r#"<w:r><w:fldChar w:fldCharType="end"/></w:r><w:r><w:t>です。</w:t></w:r></w:p>"#,
            r#"</w:body></w:document>"#,
        );
        let (p, skipped) = parse_office_paragraphs(xml, true, Format::Word).unwrap();
        assert_eq!(skipped, 2);
        assert_eq!(p.len(), 2);
        assert_eq!(p[0].plain_text(), "範囲内");
        assert_eq!(
            p[1].source(TagFormat::Anchor),
            r#"ページ <a i="1">◇</a>です。"#
        );
        let rebuilt = p[1]
            .rebuild(TagFormat::Anchor, r#"Page <a i="1">◇</a>."#, "en")
            .unwrap();
        assert!(rebuilt.contains(r#"<w:instrText xml:space="preserve"> PAGE </w:instrText>"#));
        assert!(rebuilt.contains(r#"<w:fldChar w:fldCharType="end"/>"#));
    }

    #[test]
    fn word_keeps_unclosed_ranges_locked_and_translates_nested_range_interiors() {
        let prefix = r#"<w:document xmlns:w="http://schemas.openxmlformats.org/wordprocessingml/2006/main"><w:body>"#;
        let inner = concat!(
            r#"<w:p><w:bookmarkStart w:id="11" w:name="outer"/><w:bookmarkStart w:id="12" w:name="inner"/><w:r><w:t>Heading</w:t></w:r></w:p>"#,
            r#"<w:p><w:r><w:t>Body inside both bookmarks.</w:t></w:r></w:p>"#,
            r#"<w:p><w:bookmarkEnd w:id="12"/><w:bookmarkEnd w:id="11"/><w:r><w:t>End</w:t></w:r></w:p>"#,
        );
        let xml = format!("{prefix}{inner}</w:body></w:document>");
        let (p, skipped) = parse_office_paragraphs(&xml, true, Format::Word).unwrap();
        assert_eq!(skipped, 0);
        assert_eq!(p.len(), 3);
        let mut output = xml.clone();
        output.replace_range(
            p[1].body.clone(),
            &p[1]
                .rebuild(TagFormat::Anchor, "両方の範囲内の本文。", "ja")
                .unwrap(),
        );
        assert_eq!(
            output,
            xml.replace(
                "<w:t>Body inside both bookmarks.",
                r#"<w:t xml:space="preserve">両方の範囲内の本文。"#
            )
        );
        let unclosed = xml.replace(r#"<w:bookmarkEnd w:id="11"/>"#, "");
        let (p, _) = parse_office_paragraphs(&unclosed, true, Format::Word).unwrap();
        assert!(p.is_empty());
    }

    #[test]
    fn word_heading_bookmarks_keep_their_original_edges_and_ids() {
        let xml = concat!(
            r#"<w:document xmlns:w="http://schemas.openxmlformats.org/wordprocessingml/2006/main"><w:body>"#,
            r#"<w:p><w:pPr><w:pStyle w:val="Heading1"/></w:pPr><w:bookmarkStart w:id="1" w:name="_Toc1"/><w:bookmarkStart w:id="2" w:name="_Toc2"/><w:r><w:lastRenderedPageBreak/><w:t>C</w:t></w:r><w:r><w:t>olour</w:t></w:r><w:bookmarkEnd w:id="2"/></w:p>"#,
            r#"<w:p><w:bookmarkEnd w:id="1"/><w:r><w:t>Next</w:t></w:r></w:p>"#,
            r#"<w:p><w:r><w:t>Before</w:t></w:r><w:bookmarkStart w:id="3"/><w:r><w:t>Inside</w:t></w:r></w:p>"#,
            r#"<w:p><w:r><w:t>Inside</w:t></w:r><w:bookmarkEnd w:id="3"/><w:r><w:t>After</w:t></w:r></w:p>"#,
            r#"</w:body></w:document>"#,
        );
        let (p, skipped) = parse_office_paragraphs(xml, true, Format::Word).unwrap();
        assert_eq!(skipped, 2); // Inline cross-paragraph endpoints stay protected.
        assert_eq!(p.len(), 2);
        assert_eq!(p[0].source(TagFormat::Anchor), "Colour");
        let rebuilt = p[0].rebuild(TagFormat::Anchor, "色", "ja").unwrap();
        assert_eq!(
            rebuilt,
            concat!(
                r#"<w:pPr><w:pStyle w:val="Heading1"/></w:pPr><w:bookmarkStart w:id="1" w:name="_Toc1"/><w:bookmarkStart w:id="2" w:name="_Toc2"/>"#,
                r#"<w:r><w:t xml:space="preserve">色</w:t></w:r><w:bookmarkEnd w:id="2"/>"#,
            )
        );
        assert!(
            p[1].rebuild(TagFormat::Anchor, "次", "ja")
                .unwrap()
                .starts_with(r#"<w:bookmarkEnd w:id="1"/>"#)
        );
        let tail = r#"<w:r><w:t xml:space="preserve"> </w:t></w:r><w:r><w:br/></w:r>"#;
        let with_tail = xml.replace(
            r#"<w:bookmarkEnd w:id="2"/></w:p>"#,
            &format!(r#"<w:bookmarkEnd w:id="2"/>{tail}</w:p>"#),
        );
        let (p, _) = parse_office_paragraphs(&with_tail, true, Format::Word).unwrap();
        assert_eq!(p[0].source(TagFormat::Anchor), "Colour");
        assert!(
            p[0].rebuild(TagFormat::Anchor, "色", "ja")
                .unwrap()
                .ends_with(&format!(r#"<w:bookmarkEnd w:id="2"/>{tail}"#))
        );
    }

    #[test]
    fn reports_overflow_without_changing_shape_or_paragraph_geometry() {
        let xml = concat!(
            r#"<root xmlns:a="http://schemas.openxmlformats.org/drawingml/2006/main">"#,
            r#"<sp><spPr><a:xfrm><a:ext cx="1524000" cy="889000"/></a:xfrm>"#,
            r#"<a:prstGeom prst="homePlate"/></spPr><txBody><a:bodyPr wrap="none"/>"#,
            r#"<a:p><a:pPr latinLnBrk="1"/><a:r><a:rPr sz="1300"/><a:t>星形模型</a:t></a:r></a:p></txBody></sp>"#,
            r#"<graphicFrame><xfrm><a:off x="0" y="1524000"/><a:ext cx="3048000" cy="252000"/></xfrm>"#,
            r#"<a:tbl><a:tblGrid><a:gridCol w="3048000"/></a:tblGrid><a:tr h="252000"><a:tc><a:txBody>"#,
            r#"<a:bodyPr/><a:p><a:r><a:rPr sz="1100"/><a:t>模型部品、組立・塗装、展示・片付け手順</a:t></a:r></a:p>"#,
            r#"</a:txBody><a:tcPr marT="54000" marB="36000"/></a:tc></a:tr></a:tbl></graphicFrame></root>"#,
        );
        let mut zip = ZipWriter::new(Cursor::new(Vec::new()));
        for (name, contents) in [
            ("[Content_Types].xml", "<Types/>"),
            ("ppt/presentation.xml", "<presentation/>"),
            ("ppt/slides/slide1.xml", xml),
        ] {
            zip.start_file(name, SimpleFileOptions::default()).unwrap();
            zip.write_all(contents.as_bytes()).unwrap();
        }
        let original = zip.finish().unwrap().into_inner();
        let deck = OfficeDocument::open_mode(original.clone(), true).unwrap();
        assert_eq!(
            deck.write(TagFormat::Anchor, "en", &[vec![None, None]])
                .unwrap(),
            original
        );
        let arrow = "Star-Shaped Model for the Workshop Display";
        let cell = "Model Components, Assembly and Painting, and Display and Cleanup Instructions";
        let targets = vec![Some(arrow.into()), Some(cell.into())];
        assert!(
            !deck.parts()[0]
                .overflow_frames(TagFormat::Anchor, &targets)
                .is_empty()
        );
        let out = deck.write(TagFormat::Anchor, "en", &[targets]).unwrap();
        let parsed = OfficeDocument::open(out).unwrap();
        let slide = &parsed.parts()[0];
        assert_eq!(slide.paragraphs[0].plain_text(), arrow);
        assert_eq!(slide.paragraphs[1].plain_text(), cell);
        assert!(slide.xml.contains(r#"wrap="square""#));
        assert!(slide.xml.contains(r#"latinLnBrk="1""#));
        assert!(slide.xml.contains(r#"marT="54000" marB="36000""#));
        assert!(
            slide.paragraphs[1]
                .font_size
                .is_some_and(|size| (8.0..=11.0).contains(&size))
        );
        assert!(slide.xml.contains(r#"<a:ext cx="1524000" cy="889000"/>"#));
        assert!(slide.xml.contains(r#"<a:tr h="252000">"#));
    }

    #[test]
    fn reports_overflow_beside_a_retained_paragraph_without_excessive_shrinking() {
        let xml = concat!(
            r#"<root xmlns:a="http://schemas.openxmlformats.org/drawingml/2006/main">"#,
            r#"<sp><spPr><a:xfrm><a:ext cx="2540000" cy="635000"/></a:xfrm></spPr><txBody><a:bodyPr/>"#,
            r#"<a:p><a:r><a:rPr sz="1400"/><a:t>Keep this label</a:t></a:r></a:p>"#,
            r#"<a:p><a:r><a:rPr sz="1400"/><a:t>短い文章</a:t></a:r></a:p>"#,
            r#"</txBody></sp></root>"#,
        );
        let (p, _) = paragraphs(xml).unwrap();
        let frame = layout::frames(xml).unwrap().remove(0);
        let fit = frame.fit(
            p.iter().zip([None, Some(&"This translated sentence needs several lines inside the original text box.".into())]),
            TagFormat::Anchor,
        ).unwrap();
        assert!(!fit.applies());
        assert!(fit.overflow);
        // The source paragraph remains absent from the replacement set; its
        // original 14pt height must still leave room for the translated text.
        let changed = fit
            .compact_paragraph(
                p[1].rebuild(TagFormat::Anchor, "Translated text", "en")
                    .unwrap(),
            )
            .unwrap();
        assert!(changed.contains(r#"sz="1400""#));
    }

    #[test]
    fn preserves_namespaces_relationships_and_fields_when_rebuilding() {
        let xml = concat!(
            r#"<root xmlns:d="http://schemas.openxmlformats.org/drawingml/2006/main" "#,
            r#"xmlns:r="http://schemas.openxmlformats.org/officeDocument/2006/relationships">"#,
            r#"<d:p><d:pPr lvl="1"/><d:r><d:t>Read the following </d:t></d:r>"#,
            r#"<d:r><d:rPr><d:hlinkClick r:id="rId5"/></d:rPr><d:t>report</d:t></d:r>"#,
            r#"<d:fld id="field-1" type="slidenum"><d:t>1</d:t></d:fld></d:p></root>"#,
        );
        let (found, skipped) = paragraphs(xml).unwrap();
        assert_eq!(skipped, 0);
        let p = &found[0];
        assert_eq!(
            p.source(TagFormat::Anchor),
            r#"Read the following <a i="1">report</a><a i="2">◇</a>"#
        );
        {
            let format = TagFormat::Anchor;
            let translated = format!(
                "次の{}を確認{}",
                format.wrap(1, "報告書", false),
                format.wrap(2, "", true)
            );
            assert!(!p.valid(
                format,
                &format!("次の{}を確認", format.wrap(1, "報告書", false))
            ));
            assert!(!p.valid(format, &format!("{translated}{}", format.wrap(2, "", true))));
            let replacement = p.rebuild(format, &translated, "ja").unwrap();
            assert!(replacement.contains(r#"<d:hlinkClick r:id="rId5"/>"#));
            assert!(
                replacement.contains(r#"<d:fld id="field-1" type="slidenum"><d:t>1</d:t></d:fld>"#)
            );
            let mut updated = xml.to_owned();
            updated.replace_range(p.body.clone(), &replacement);
            assert_eq!(
                paragraphs(&updated).unwrap().0[0].source(format),
                translated
            );
        }
    }
    #[test]
    fn styled_whitespace_round_trips_as_an_opaque_run() {
        let xml = concat!(
            r#"<a:p xmlns:a="http://schemas.openxmlformats.org/drawingml/2006/main">"#,
            r#"<a:r><a:t>Read the following</a:t></a:r>"#,
            r#"<a:r><a:rPr sz="1400"/><a:t>&#160;　</a:t></a:r>"#,
            r#"<a:r><a:t>report</a:t></a:r></a:p>"#,
        );
        let (found, skipped) = paragraphs(xml).unwrap();
        assert_eq!(skipped, 0);
        let paragraph = &found[0];
        assert_eq!(
            paragraph.source(TagFormat::Anchor),
            r#"Read the following<a i="1">◇</a>report"#
        );
        let out = paragraph
            .rebuild(
                TagFormat::Anchor,
                r#"Read the following<a i="1">◇</a>report"#,
                "",
            )
            .unwrap();
        assert!(out.contains("<a:rPr sz=\"1400\"/><a:t>\u{a0}　</a:t>"));
    }

    #[test]
    fn simplified_word_paragraph_uses_base_style_and_preserves_boundary_atoms() {
        let leading = r#"<w:r><w:tab/></w:r>"#;
        let trailing = concat!(
            r#"<w:r><w:br/><w:footnoteReference w:id="3"/><w:endnoteReference w:id="4"/></w:r>"#,
            r#"<w:fldSimple w:instr="PAGE"><w:r><w:t>1</w:t></w:r></w:fldSimple>"#,
            r#"<w:r><w:drawing><wp:inline/></w:drawing></w:r>"#,
        );
        let text = r#"<w:r><w:t>A sample paragraph with </w:t></w:r><w:r><w:rPr><w:b/></w:rPr><w:t>style</w:t></w:r>"#;
        let xml = format!(
            r#"<w:document xmlns:w="http://schemas.openxmlformats.org/wordprocessingml/2006/main" xmlns:wp="http://schemas.openxmlformats.org/drawingml/2006/wordprocessingDrawing"><w:body><w:p><w:pPr><w:pStyle w:val="Normal"/></w:pPr>{leading}{text}{trailing}</w:p></w:body></w:document>"#
        );
        let document = OfficeDocument::open_format(word_package(&xml), true, Format::Word).unwrap();
        let part = &document.parts()[0];
        let paragraph = &part.paragraphs[0];
        let format = TagFormat::Anchor;
        assert_eq!(
            part.anchor_fallback_source(0).as_deref(),
            Some("A sample paragraph with style")
        );
        assert!(!paragraph.valid(format, "例文です。"));
        assert!(
            paragraph
                .simplified_target(format, "A sample paragraph with style".into(), "ja")
                .is_none()
        );
        assert!(
            paragraph
                .simplified_target(format, "例文です。◇".into(), "ja")
                .is_none()
        );
        let target = paragraph
            .simplified_target(format, "例文です。".into(), "ja")
            .unwrap();
        let output = document.write(format, "ja", &[vec![Some(target)]]).unwrap();
        let reopened = OfficeDocument::open_format(output, true, Format::Word).unwrap();
        let output_xml = &reopened.parts()[0].xml;
        assert!(output_xml.contains(r#"<w:pStyle w:val="Normal"/>"#));
        assert!(!output_xml.contains("<w:b/>"));
        let new_text = output_xml.find("例文です。").unwrap();
        assert!(output_xml[..new_text].contains(leading));
        for atom in [
            "<w:br/>",
            r#"<w:footnoteReference w:id="3"/>"#,
            r#"<w:endnoteReference w:id="4"/>"#,
            r#"<w:fldSimple w:instr="PAGE"><w:r><w:t>1</w:t></w:r></w:fldSimple>"#,
            "<w:drawing><wp:inline/></w:drawing>",
        ] {
            assert!(output_xml[new_text..].contains(atom), "{atom}");
            assert_eq!(output_xml.matches(atom).count(), 1, "{atom}");
        }
    }

    #[test]
    fn simplified_word_paragraph_keeps_source_for_shifted_or_hidden_runs() {
        for property in [
            r#"<w:vertAlign w:val="superscript"/>"#,
            r#"<w:vertAlign w:val="subscript"/>"#,
            r#"<w:position w:val="-4"/>"#,
            "<w:vanish/>",
            "<w:webHidden/>",
        ] {
            let xml = format!(
                r#"<w:p xmlns:w="http://schemas.openxmlformats.org/wordprocessingml/2006/main"><w:r><w:t>Area is 10</w:t></w:r><w:r><w:rPr>{property}</w:rPr><w:t>6</w:t></w:r><w:r><w:rPr><w:b/></w:rPr><w:t> square units</w:t></w:r></w:p>"#
            );
            let (paragraphs, _) = parse_office_paragraphs(&xml, true, Format::Word).unwrap();
            let paragraph = &paragraphs[0];
            // The bold run still needs an anchor even if amount protection combined the digits.
            assert!(paragraph.has_tags());
            assert!(paragraph.anchor_fallback_source().is_none(), "{property}");
            assert!(
                paragraph
                    .simplified_target(TagFormat::Anchor, "106 unités carrées".into(), "fr")
                    .is_none()
            );
        }
    }

    #[test]
    fn simplified_powerpoint_paragraph_keeps_source_for_shifted_or_hidden_runs() {
        for (properties, eligible) in [
            (r#"<a:rPr baseline="30000"/>"#, false),
            (r#"<a:rPr baseline="-25000"/>"#, false),
            ("<a:rPr><a:noFill/></a:rPr>", false),
            (r#"<a:rPr baseline="0"/>"#, true),
        ] {
            let xml = format!(
                r#"<a:p xmlns:a="http://schemas.openxmlformats.org/drawingml/2006/main"><a:r><a:t>Area is 10</a:t></a:r><a:r>{properties}<a:t>6</a:t></a:r><a:r><a:rPr b="1"/><a:t> square units</a:t></a:r></a:p>"#
            );
            let (paragraphs, _) = parse_office_paragraphs(&xml, true, Format::PowerPoint).unwrap();
            let paragraph = &paragraphs[0];
            assert!(paragraph.has_tags());
            assert_eq!(
                paragraph.anchor_fallback_source().is_some(),
                eligible,
                "{properties}"
            );
            assert_eq!(
                paragraph
                    .simplified_target(TagFormat::Anchor, "106 unités carrées".into(), "fr")
                    .is_some(),
                eligible
            );
        }
    }

    #[test]
    fn simplified_powerpoint_paragraph_keeps_source_for_a_private_use_symbol() {
        let xml = concat!(
            r#"<a:p xmlns:a="http://schemas.openxmlformats.org/drawingml/2006/main">"#,
            r#"<a:r><a:rPr><a:sym typeface="Symbol"/></a:rPr><a:t>"#,
            "\u{f0b7}",
            r#"</a:t></a:r><a:r><a:t>File requirements</a:t></a:r></a:p>"#,
        );
        let (paragraphs, _) = parse_office_paragraphs(xml, true, Format::PowerPoint).unwrap();
        let paragraph = &paragraphs[0];
        assert!(paragraph.has_tags());
        assert!(paragraph.anchor_fallback_source().is_none());
        assert!(
            paragraph
                .simplified_target(TagFormat::Anchor, "必要なファイル".into(), "ja")
                .is_none()
        );
    }

    #[test]
    fn simplified_office_paragraph_refuses_an_interior_break_or_link() {
        let format = TagFormat::Anchor;
        let text = r#"<a:r><a:t>Read the document</a:t></a:r>"#;
        for (before, after) in [
            ("", r#"<a:br/><a:r><a:t>page two</a:t></a:r>"#),
            (
                "",
                r#"<a:r><a:rPr><a:hlinkClick r:id="rId1"/></a:rPr><a:t>the guide</a:t></a:r>"#,
            ),
            (
                r#"<a:r><a:rPr><a:hlinkMouseOver r:id="rId1"/></a:rPr><a:t>the guide</a:t></a:r>"#,
                "",
            ),
        ] {
            let xml = format!(
                r#"<a:p xmlns:a="http://schemas.openxmlformats.org/drawingml/2006/main" xmlns:r="http://schemas.openxmlformats.org/officeDocument/2006/relationships">{before}{text}{after}</a:p>"#
            );
            let (paragraphs, _) = parse_office_paragraphs(&xml, true, Format::PowerPoint).unwrap();
            let paragraph = &paragraphs[0];
            assert!(paragraph.has_tags());
            assert!(paragraph.anchor_fallback_source().is_none());
            assert!(
                paragraph
                    .simplified_target(format, "説明を読む".into(), "ja")
                    .is_none()
            );
        }
    }
}
