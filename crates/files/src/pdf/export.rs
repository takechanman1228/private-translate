//! Shared application/probe pipeline. Each PDFium operation takes and releases its own lock.
use super::inplace::{self, Options, Outcome, Retain, Status};
use std::collections::HashMap;

pub struct Export {
    pub bytes: Vec<u8>,
    pub outcomes: Vec<Outcome>,
    pub erasure: super::erase::Report,
}

/// Plan, erase the source text conservatively, replan failed paragraphs, and insert only after
/// verifying the erased and inserted paragraph sets. No path of a page is taken off or drawn.
/// Call without holding the PDFium lock. Input translations are never modified.
pub fn write(
    bytes: &[u8],
    pages: Option<usize>,
    translations: &HashMap<String, String>,
    options: Options,
) -> Result<Export, crate::FileError> {
    run(
        bytes,
        pages,
        translations,
        options,
        false,
        &mut super::erase::Blanks::default(),
    )
}

/// Developer inspection of the very same erasure pipeline, without inserting text.
pub fn erase_only(
    bytes: &[u8],
    pages: Option<usize>,
    translations: &HashMap<String, String>,
    options: Options,
) -> Result<Export, crate::FileError> {
    run(
        bytes,
        pages,
        translations,
        options,
        true,
        &mut super::erase::Blanks::default(),
    )
}

fn run(
    bytes: &[u8],
    pages: Option<usize>,
    translations: &HashMap<String, String>,
    options: Options,
    erase_only: bool,
    blanks: &mut super::erase::Blanks,
) -> Result<Export, crate::FileError> {
    let mut map = translations.clone();
    let mut failed = std::collections::BTreeMap::new();
    let (mut planned, mut chars) = inplace::erase_plan(bytes, pages, &map, options)?;
    let (clean, report) = loop {
        // Each replan erases from `bytes` again: `blanks` keeps what its glyphs paint.
        let (clean, report) = super::erase::erase_with(bytes, &chars, blanks)?;
        let retained = report.retained.clone();
        if retained.is_empty() {
            break (clean, report);
        }
        let affected: std::collections::HashSet<_> = planned
            .iter()
            .filter(|o| retained.contains_key(&o.paragraph.id))
            .map(|o| o.paragraph.page)
            .collect();
        let mut removed = false;
        for (id, reason) in retained {
            removed |= map.remove(&id).is_some();
            failed.insert(id, reason);
        }
        if !removed {
            return Err(crate::FileError::PdfPlan);
        }
        let (replanned, selected) = inplace::erase_plan_pages(bytes, &affected, &map, options)?;
        let mut replanned: HashMap<_, _> = replanned
            .into_iter()
            .map(|o| (o.paragraph.id.clone(), o))
            .collect();
        for outcome in &mut planned {
            if let Some(replacement) = replanned.remove(&outcome.paragraph.id) {
                *outcome = replacement;
            }
        }
        chars.retain(|c| !affected.contains(&c.page));
        chars.extend(selected);
        chars.sort_by_key(|c| (c.page, c.index));
    };
    let mut report = report;
    report.retained = failed.clone();
    let (bytes, outcomes) = if erase_only {
        for o in &mut planned {
            if failed.contains_key(&o.paragraph.id) {
                o.status = Status::Retain(Retain::Erase);
            }
            o.translation = (o.status == Status::Translate).then(String::new);
            o.drawn = None;
            o.drawn_lines = 0;
            o.line_rects.clear();
        }
        (clean, planned)
    } else {
        let (out, mut outcomes) =
            inplace::write_checked_erased(bytes, &clean, pages, &map, options, &report.erased)?;
        // A paragraph's one link goes to its translation. Failing that, it stays.
        let links = inplace::followed_links(bytes, &outcomes, pages)?;
        let out = match super::erase::move_links(&out, &links) {
            Ok((moved, pages)) => {
                report.diagnostics.extend(
                    pages
                        .iter()
                        .map(|p| format!("page {}: link moved to its translation", p + 1)),
                );
                moved
            }
            Err(_) => out,
        };
        for o in &mut outcomes {
            if failed.contains_key(&o.paragraph.id) {
                o.status = Status::Retain(Retain::Erase);
                if let Some(plan) = planned.iter().find(|p| p.paragraph.id == o.paragraph.id) {
                    o.attempts = plan.attempts.clone();
                }
            }
        }
        (out, outcomes)
    };
    Ok(Export {
        bytes,
        outcomes,
        erasure: report,
    })
}

/// Validated input for layout translation. The ordinary reader supplies the same
/// limits, protection checks and review flags as the existing file tab.
pub struct Document {
    source: super::Pdf,
    analysis: inplace::Analysis,
    paragraphs: Vec<inplace::Paragraph>,
    units: Vec<super::Unit>,
    limit: Option<usize>,
    cache: std::sync::Mutex<Option<CachedOutput>>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum PageKind {
    Layout,
    Supplement,
    Bilingual,
}
impl PageKind {
    pub fn name(self) -> &'static str {
        match self {
            Self::Layout => "layout",
            Self::Supplement => "supplement",
            Self::Bilingual => "bilingual",
        }
    }
}

#[derive(Clone, Debug)]
pub struct PageResult {
    /// Original and output page numbers, starting at one.
    pub page: usize,
    pub output_page: usize,
    pub kind: PageKind,
    pub added_pages: usize,
    pub target_characters: usize,
    pub placed_characters: usize,
    pub table_characters: usize,
    pub reasons: Vec<&'static str>,
    pub placed: Vec<String>,
    pub appended: Vec<String>,
    /// Every repeated input has an explicit destination on the first source page.
    pub repeated: Vec<RepeatedTranslation>,
    /// Representatives printed here once; not counted again as ordinary appended IDs.
    pub repeated_added: Vec<String>,
    /// Missing/empty or source-identical model responses, never counted as placed.
    pub untranslated: Vec<String>,
    pub figure_characters: usize,
    pub all_characters: usize,
    pub unplaced_label_characters: usize,
    /// Eligible characters without a translation, across prose and labels.
    pub untranslated_characters: usize,
    /// Eligible characters placed before any whole-page fallback.
    pub all_placed_characters: usize,
    pub small_paragraphs: Vec<String>,
    pub has_square_markers: bool,
}

#[derive(Clone, Debug)]
pub struct RepeatedTranslation {
    pub id: String,
    pub representative: String,
    /// One-based original page whose additional pages contain this translation.
    pub first_page: usize,
    /// First additional output page; the item may continue on following sheets.
    pub output_page: usize,
}

pub struct DocumentExport {
    pub bytes: Vec<u8>,
    pub pages: Vec<PageResult>,
    /// Placement before page fallback, for developer audits only.
    pub placement: Option<Export>,
    pub fallback_error: Option<String>,
}

/// Retain only the final bytes and summary, not the large placement audit.
pub struct SavedDocument {
    pub bytes: Vec<u8>,
    pub pages: Vec<PageResult>,
    pub fallback_error: Option<String>,
}
struct CachedOutput {
    targets: Vec<Option<String>>,
    language: String,
    result: std::sync::Arc<SavedDocument>,
}
fn guarded<T>(f: impl FnOnce() -> Result<T, crate::FileError>) -> Result<T, crate::FileError> {
    std::panic::catch_unwind(std::panic::AssertUnwindSafe(f)).unwrap_or_else(|panic| {
        let detail = panic
            .downcast_ref::<String>()
            .map(String::as_str)
            .or_else(|| panic.downcast_ref::<&str>().copied())
            .unwrap_or("non-string panic payload");
        // Propagate the captured detail to the app's existing app.log error path.
        Err(crate::FileError::PdfPanic(detail.to_owned()))
    })
}

impl Document {
    pub fn read(path: &std::path::Path) -> Result<Self, crate::FileError> {
        Self::read_pages(path, None)
    }
    /// A page limit is for repeatable developer samples, never used by the app.
    pub fn read_pages(
        path: &std::path::Path,
        limit: Option<usize>,
    ) -> Result<Self, crate::FileError> {
        guarded(|| Self::read_inner(path, limit))
    }
    fn read_inner(path: &std::path::Path, limit: Option<usize>) -> Result<Self, crate::FileError> {
        let source = super::Pdf::read(path)?;
        let analysis = inplace::analyze(&source.bytes, limit)?;
        let paragraphs: Vec<_> = analysis
            .analyzed
            .iter()
            .flat_map(|p| &p.paragraphs)
            .filter(|p| p.translation_candidate())
            .cloned()
            .collect();
        let units = paragraphs
            .iter()
            .map(|p| super::Unit {
                text: p.text.clone(),
                page: p.page + 1,
            })
            .collect();
        Ok(Self {
            source,
            analysis,
            paragraphs,
            units,
            limit,
            cache: std::sync::Mutex::new(None),
        })
    }
    pub fn units(&self) -> &[super::Unit] {
        &self.units
    }
    pub fn paragraphs(&self) -> &[inplace::Paragraph] {
        &self.paragraphs
    }
    pub fn check_amounts(&self, targets: &mut [Option<String>], language: &str) {
        for (p, t) in self.paragraphs.iter().zip(targets) {
            if t.as_ref()
                .is_some_and(|t| !crate::office::amounts::pdf_matches(&p.text, t, language))
            {
                *t = None;
            }
        }
    }
    pub fn applied_units(&self, targets: &[Option<String>]) -> Vec<usize> {
        self.units
            .iter()
            .zip(targets)
            .enumerate()
            .filter_map(|(i, (p, t))| {
                t.as_ref()
                    .filter(|t| !t.trim().is_empty() && !same_text(t, &p.text))
                    .map(|_| i)
            })
            .collect()
    }
    pub fn review_pages(&self, targets: &[Option<String>]) -> Vec<usize> {
        let mut pages: Vec<_> = self
            .source
            .pages
            .iter()
            .enumerate()
            .filter_map(|(i, p)| (!p.has_text || p.skipped).then_some(i + 1))
            .collect();
        pages.extend(self.units.iter().zip(targets).filter_map(|(p, t)| {
            t.as_ref()
                .is_none_or(|t| t.trim().is_empty())
                .then_some(p.page)
        }));
        pages.sort_unstable();
        pages.dedup();
        pages
    }
    pub fn write(
        &self,
        targets: &[Option<String>],
        language: &str,
    ) -> Result<DocumentExport, crate::FileError> {
        if !super::supports_pdf(language) {
            return Err(super::Error::UnsupportedLanguage.into());
        }
        guarded(|| self.write_inner(targets, language))
    }
    pub fn write_cached(
        &self,
        targets: &[Option<String>],
        language: &str,
    ) -> Result<std::sync::Arc<SavedDocument>, crate::FileError> {
        let mut cache = self.cache.lock().unwrap_or_else(|e| e.into_inner());
        if let Some(c) = &*cache
            && c.targets == targets
            && c.language == language
        {
            return Ok(c.result.clone());
        }
        let export = self.write(targets, language)?;
        let result = std::sync::Arc::new(SavedDocument {
            bytes: export.bytes,
            pages: export.pages,
            fallback_error: export.fallback_error,
        });
        *cache = Some(CachedOutput {
            targets: targets.to_vec(),
            language: language.into(),
            result: result.clone(),
        });
        Ok(result)
    }
    fn write_inner(
        &self,
        targets: &[Option<String>],
        language: &str,
    ) -> Result<DocumentExport, crate::FileError> {
        if targets.len() != self.paragraphs.len() {
            return Err(crate::FileError::MismatchedResults);
        }
        let map: HashMap<_, _> = self
            .paragraphs
            .iter()
            .zip(targets)
            .filter_map(|(p, t)| {
                t.as_ref()
                    .filter(|t| !t.trim().is_empty() && !same_text(t, &p.text))
                    .map(|t| (p.id.clone(), t.clone()))
            })
            .collect();
        // Excluding a paragraph means replanning from the original bytes, never
        // keeping an erasure without its replacement. The loop only removes IDs.
        let mut placement_map = map.clone();
        let mut small = std::collections::HashSet::new();
        let budget = super::parser::budget();
        // What the source's glyphs paint is told once for the whole save, however many
        // times it is written again without some paragraphs.
        let mut blanks = super::erase::Blanks::default();
        let (placement, mut fallback_error) = loop {
            match guarded(|| {
                budget
                    .run(|| {
                        run(
                            &self.source.bytes,
                            self.limit,
                            &placement_map,
                            Options {
                                family: super::Family::for_language(language),
                                ..Options::default()
                            },
                            false,
                            &mut blanks,
                        )
                    })
                    .map_err(super::erase::Error::Pdf)?
            }) {
                Ok(p) => {
                    let rejected: Vec<_> = p
                        .outcomes
                        .iter()
                        .filter(|o| {
                            o.status == Status::Translate
                                && o.scale.is_some_and(|s| {
                                    s * o.paragraph.size < 6.0 - 0.00001 || s < 0.6 - 0.00001
                                })
                        })
                        .map(|o| o.paragraph.id.clone())
                        .collect();
                    if rejected.is_empty() {
                        break (Some(p), None);
                    }
                    for id in rejected {
                        placement_map.remove(&id);
                        small.insert(id);
                    }
                }
                Err(e) => break (None, Some(e.to_string())),
            }
        };
        let audit = if let Some(p) = &placement {
            match guarded(|| {
                Ok(inplace::check(
                    &self.source.bytes,
                    &p.bytes,
                    &p.outcomes,
                    self.limit,
                    1.25,
                )?)
            }) {
                Ok(a) => Some(a),
                Err(e) => {
                    fallback_error = Some(e.to_string());
                    None
                }
            }
        } else {
            None
        };
        let mut pages = Vec::new();
        for page in 0..self.analysis.pages {
            let untranslated: Vec<_> = self
                .paragraphs
                .iter()
                .zip(targets)
                .filter(|(p, t)| {
                    p.page == page
                        && t.as_ref()
                            .is_none_or(|t| t.trim().is_empty() || same_text(t, &p.text))
                })
                .map(|(p, _)| p.id.clone())
                .collect();
            let mut candidates: Vec<_> =
                self.paragraphs.iter().filter(|p| p.page == page).collect();
            reading_order(&mut candidates);
            let outcomes: Vec<_> = placement
                .as_ref()
                .map(|p| {
                    p.outcomes
                        .iter()
                        .filter(|o| o.paragraph.page == page)
                        .collect()
                })
                .unwrap_or_default();
            // Source-identical identifiers are exempt only when label-like.
            let eligible = |p: &inplace::Paragraph| {
                !(p.figure && untranslated.contains(&p.id) && identifier_only(&p.text))
            };
            let placed_here = |p: &inplace::Paragraph| {
                outcomes
                    .iter()
                    .any(|o| o.paragraph.id == p.id && o.status == Status::Translate)
            };
            let count = |predicate: &dyn Fn(&inplace::Paragraph) -> bool| -> usize {
                candidates
                    .iter()
                    .filter(|p| eligible(p) && predicate(p))
                    .map(|p| character_count(&p.text))
                    .sum()
            };
            // Every prose character stays in the denominator. Source-identical,
            // missing and empty translations never enter placement_map, so both
            // untranslated prose and failed placements count against the same
            // 80% threshold. In particular, >20% untranslated prose must fail;
            // a minority untranslated paragraph alone does not reject the page.
            let target_characters = count(&|p| p.prose);
            let figure_characters = count(&|p| p.figure);
            let all_characters = count(&|_| true);
            let unplaced_label_characters = count(&|p| p.figure && !placed_here(p));
            let untranslated_characters = count(&|p| untranslated.contains(&p.id));
            let all_placed_characters = count(&placed_here);
            let table_characters = count(&|p| p.table);
            let placed_characters = count(&|p| p.prose && placed_here(p));
            let mut reasons = Vec::new();
            // Whole-page budgets prevent prose and labels from independently
            // accepting a page whose combined untranslated/unplaced share is high.
            // Formula-bearing paragraphs remain eligible. Missing and empty
            // responses count just like source-identical responses.
            if untranslated_characters * 100 > all_characters * 20 {
                reasons.push("untranslated-page");
            }
            if all_placed_characters * 100 < all_characters * 50 {
                reasons.push("page-coverage");
            }
            if target_characters > 0 && placed_characters * 100 < target_characters * 80 {
                reasons.push("coverage");
            }
            if unplaced_label_characters * 100 > all_characters * 50 {
                reasons.push("unplaced-label-majority");
            }
            if target_characters == 0
                && figure_characters > 0
                && (figure_characters - unplaced_label_characters) * 100 < figure_characters * 50
            {
                reasons.push("labels-only");
            }
            // Long punctuation-free fragments are neither short labels nor prose.
            // Keep their supplied translation in the supplement; an untranslated
            // fragment must not disappear from the page decision.
            if candidates
                .iter()
                .any(|p| eligible(p) && !p.prose && !p.figure && untranslated.contains(&p.id))
            {
                reasons.push("untranslated-long-fragment");
            }
            if all_characters > 0 && table_characters * 100 >= all_characters * 30 {
                reasons.push("table");
            }
            // Numerical table rows may be deliberately outside the translation
            // denominator. Their headers still cannot be reflowed as ordinary prose.
            let numeric_rows = self.analysis.analyzed.get(page).map_or(0, |info| {
                info.paragraphs
                    .iter()
                    .filter(|p| {
                        p.table
                            && p.text.chars().filter(|c| c.is_numeric()).count() * 2
                                >= character_count(&p.text)
                            && p.text.chars().filter(|c| c.is_numeric()).count() >= 4
                    })
                    .count()
            });
            let multirow_table = self.analysis.analyzed.get(page).is_some_and(|info| {
                info.paragraphs.iter().any(|p| {
                    p.table && p.lines > 1 && p.text.chars().filter(|c| c.is_numeric()).count() >= 8
                })
            });
            if table_characters > 0 && (numeric_rows >= 2 || multirow_table) {
                reasons.push("numeric-table");
            }
            if outcomes
                .iter()
                .any(|o| o.status == Status::Retain(Retain::PlanMismatch))
            {
                reasons.push("plan-mismatch");
            }
            if !candidates.is_empty()
                && audit
                    .as_ref()
                    .is_none_or(|a| !page_passes(a, page, &outcomes))
            {
                reasons.push("verification");
            }
            let bilingual = !reasons.is_empty();
            let mut placed = Vec::new();
            let mut appended = Vec::new();
            for p in candidates {
                if !map.contains_key(&p.id) {
                    continue;
                }
                if !bilingual
                    && outcomes
                        .iter()
                        .any(|o| o.paragraph.id == p.id && o.status == Status::Translate)
                {
                    placed.push(p.id.clone());
                } else {
                    appended.push(p.id.clone());
                }
            }
            let kind = if bilingual {
                PageKind::Bilingual
            } else if !appended.is_empty() {
                PageKind::Supplement
            } else {
                PageKind::Layout
            };
            pages.push(PageResult {
                page: page + 1,
                output_page: 0,
                kind,
                added_pages: 0,
                target_characters,
                placed_characters: if bilingual { 0 } else { placed_characters },
                table_characters,
                reasons,
                placed,
                appended,
                repeated: Vec::new(),
                repeated_added: Vec::new(),
                untranslated,
                figure_characters,
                all_characters,
                unplaced_label_characters,
                untranslated_characters,
                all_placed_characters,
                small_paragraphs: self
                    .paragraphs
                    .iter()
                    .filter(|p| p.page == page && small.contains(&p.id))
                    .map(|p| p.id.clone())
                    .collect(),
                has_square_markers: false,
            });
        }
        // Decide destinations once, after placement and whole-page fallback. Only
        // occurrences actually going to an added page may be consolidated. Every
        // member matches the fixed representative (3pt is not transitive).
        let mut assigned = std::collections::HashSet::new();
        for p in &self.paragraphs {
            if assigned.contains(&p.id)
                || !pages[p.page].appended.contains(&p.id)
                || !self.running_matter(p)
            {
                continue;
            }
            let group: Vec<_> = self
                .paragraphs
                .iter()
                .filter(|q| {
                    !assigned.contains(&q.id)
                        && pages[q.page].appended.contains(&q.id)
                        && self.running_matter(q)
                        && same_margin_line(p, q)
                        && map.get(&p.id) == map.get(&q.id)
                })
                .collect();
            if group.len() < 2 {
                continue;
            }
            for q in group {
                assigned.insert(q.id.clone());
                pages[q.page].appended.retain(|id| id != &q.id);
                pages[q.page].repeated.push(RepeatedTranslation {
                    id: q.id.clone(),
                    representative: p.id.clone(),
                    first_page: p.page + 1,
                    output_page: 0,
                });
            }
            pages[p.page].repeated_added.push(p.id.clone());
        }
        for page in &mut pages {
            if page.kind != PageKind::Bilingual {
                page.kind = if page.appended.is_empty() && page.repeated_added.is_empty() {
                    PageKind::Layout
                } else {
                    PageKind::Supplement
                };
            }
        }
        let bytes = self.assemble(&map, language, placement.as_ref(), &mut pages)?;
        let output_pages: Vec<_> = pages.iter().map(|p| p.output_page + 1).collect();
        for page in &mut pages {
            for item in &mut page.repeated {
                item.output_page = output_pages[item.first_page - 1];
            }
        }
        Ok(DocumentExport {
            bytes,
            pages,
            placement,
            fallback_error,
        })
    }

    fn running_matter(&self, p: &inplace::Paragraph) -> bool {
        let Some(page) = self.analysis.analyzed.get(p.page) else {
            return false;
        };
        if p.rect.top > page.height * 0.12 && p.rect.bottom < page.height * 0.88 {
            return false;
        }
        repeated_margin_line(p, &self.paragraphs, self.analysis.pages)
    }

    fn assemble(
        &self,
        translations: &HashMap<String, String>,
        language: &str,
        placement: Option<&Export>,
        pages: &mut [PageResult],
    ) -> Result<Vec<u8>, crate::FileError> {
        use pdfium_render::prelude::*;
        let face = super::Faces::new(
            &self.source.font,
            &self.source.noto,
            super::Family::for_language(language),
        )?;
        let mut text_lines = Vec::new();
        // The lock is scoped to assembly. Unicode encoding below uses lopdf only.
        let bytes = {
            let (_guard, pdfium) = super::pdfium()?;
            let original = pdfium
                .load_pdf_from_byte_slice(&self.source.bytes, None)
                .map_err(super::Error::from)?;
            let mut output = pdfium
                .load_pdf_from_byte_slice(placement.map_or(&self.source.bytes, |p| &p.bytes), None)
                .map_err(super::Error::from)?;
            let mut added = 0;
            let mut repeated_pages: HashMap<String, Vec<usize>> = HashMap::new();
            for info in pages.iter() {
                for r in &info.repeated {
                    repeated_pages
                        .entry(r.representative.clone())
                        .or_default()
                        .push(info.page);
                }
            }
            for info in pages {
                let index = (info.page - 1 + added) as u16;
                info.output_page = index as usize + 1;
                if info.kind == PageKind::Bilingual {
                    output
                        .pages()
                        .get(index)
                        .map_err(super::Error::from)?
                        .delete()
                        .map_err(super::Error::from)?;
                    output
                        .pages_mut()
                        .copy_page_from_document(&original, (info.page - 1) as u16, index)
                        .map_err(super::Error::from)?;
                }
                let mut lines = Vec::new();
                for id in info.appended.iter().chain(&info.repeated_added) {
                    let p = self
                        .paragraphs
                        .iter()
                        .find(|p| &p.id == id)
                        .ok_or(crate::FileError::PdfPlan)?;
                    if info.kind == PageKind::Supplement {
                        let excerpt: String = p.text.chars().take(60).collect();
                        let prefix = format!(
                            "{} {excerpt}",
                            if language == "ja" {
                                "原文："
                            } else {
                                "Source:"
                            }
                        );
                        for chunk in super::wrap(&prefix, &face, super::CONTENT) {
                            lines.push(vec![(super::MARGIN, chunk.to_owned())]);
                        }
                    }
                    let text = &translations[id];
                    let text = text.split_whitespace().collect::<Vec<_>>().join(" ");
                    for chunk in super::wrap(&text, &face, super::CONTENT) {
                        lines.push(vec![(super::MARGIN, chunk.to_owned())]);
                    }
                    if info.repeated_added.contains(id) {
                        let mut source_pages = repeated_pages[id].clone();
                        source_pages.sort_unstable();
                        source_pages.dedup();
                        let source_pages = source_pages
                            .iter()
                            .map(usize::to_string)
                            .collect::<Vec<_>>()
                            .join(", ");
                        let note = if language == "ja" {
                            format!("（原本 {source_pages} ページの同じ見出しも同じ訳文）")
                        } else {
                            format!(
                                "(The same translation applies to this heading on original pages {source_pages}.)"
                            )
                        };
                        for chunk in super::wrap(&note, &face, super::CONTENT) {
                            lines.push(vec![(super::MARGIN, chunk.to_owned())]);
                        }
                    }
                    lines.push(Vec::new());
                }
                for mut sheet in super::translation_sheets(
                    &lines,
                    (info.page - 1, info.page - 1),
                    language == "ja",
                    true,
                    info.kind == PageKind::Supplement,
                ) {
                    // Reuse the existing note row; pagination and the default
                    // bilingual writer remain unchanged.
                    let markers = sheet.iter().skip(2).any(|(_, _, _, text)| {
                        text.chars()
                            .any(|ch| ch == '□' || face.glyph(ch, true).is_none())
                    });
                    if markers {
                        info.has_square_markers = true;
                        if let Some(note) = sheet.get_mut(1) {
                            note.2 = if language == "ja" { 9.0 } else { 8.0 };
                            note.3 = if language == "ja" {
                                "□ は、このPDFのフォントで表示できない文字、または数式の位置を示します。"
                            } else {
                                "□ marks a character unavailable in this PDF's font, or the position of a formula."
                            }.into();
                        }
                    }
                    let n = info.output_page + info.added_pages;
                    output
                        .pages_mut()
                        .create_page_at_index(
                            PdfPagePaperSize::from_points(
                                super::pt(super::WIDTH),
                                super::pt(super::HEIGHT),
                            ),
                            n as u16,
                        )
                        .map_err(super::Error::from)?;
                    for (x, y, size, text) in sheet {
                        text_lines.push(super::unicode::TextLine::at(n, x, y, size, text));
                    }
                    info.added_pages += 1;
                    added += 1;
                }
            }
            output.save_to_bytes().map_err(super::Error::from)?
        };
        Ok(super::unicode::append_faces(
            &bytes,
            &face,
            &text_lines,
            true,
        )?)
    }
}

/// Repetition authorizes consolidation, never deletion. Keep exact source text
/// (apart from whitespace) and location; page numbers are not interchangeable.
fn same_margin_line(p: &inplace::Paragraph, q: &inplace::Paragraph) -> bool {
    p.lines == 1
        && q.lines == 1
        && p.text.split_whitespace().eq(q.text.split_whitespace())
        && (p.rect.left - q.rect.left).abs() < 3.0
        && (p.rect.top - q.rect.top).abs() < 3.0
}

fn repeated_margin_line(
    p: &inplace::Paragraph,
    paragraphs: &[inplace::Paragraph],
    pages: usize,
) -> bool {
    let matching_pages: std::collections::HashSet<_> = paragraphs
        .iter()
        .filter(|q| same_margin_line(p, q))
        .map(|q| q.page)
        .collect();
    matching_pages.len() >= pages.div_ceil(4).max(3)
}

/// Only obvious nonlinguistic identifiers are exempt; names/prose are counted
/// conservatively because an unchanged response is not proof of a translation.
fn identifier_only(text: &str) -> bool {
    let t = text.trim();
    !t.chars().any(char::is_alphabetic)
        || (!t.contains(char::is_whitespace)
            && (t.contains('@') || t.starts_with("https://") || t.starts_with("http://")))
}

fn character_count(text: &str) -> usize {
    text.chars().filter(|c| !c.is_whitespace()).count()
}

fn page_passes(a: &inplace::Check, page: usize, outcomes: &[&Outcome]) -> bool {
    let belongs = |id: &String| {
        outcomes
            .iter()
            .any(|o| id.split(':').next() == Some(o.paragraph.id.as_str()))
    };
    a.accounted()
        && a.pages_original == a.pages_output
        && a.pages.iter().find(|p| p.page == page).is_some_and(|p| {
            p.changed_outside <= 0.001
                && p.darkened_outside == 0
                && p.lightened_outside == 0
                && p.paths_changed == 0
        })
        && [
            &a.text.missing,
            &a.text.remaining,
            &a.text.unverified,
            &a.text.lost,
            &a.text.kept_unverified,
            &a.text.unplaced,
            &a.crossings,
            &a.occupied_lines,
            &a.invisible_lines,
        ]
        .iter()
        .all(|ids| !ids.iter().any(belongs))
        && !a.overlaps.iter().any(|(x, y)| belongs(x) || belongs(y))
}

fn normalized_text(text: &str) -> String {
    text.chars()
        .filter(|c| !c.is_whitespace())
        .flat_map(char::to_lowercase)
        .collect()
}
fn same_text(a: &str, b: &str) -> bool {
    normalized_text(a) == normalized_text(b)
}

/// Recursive whitespace cuts: columns left to right, then horizontal bands top to bottom.
fn reading_order(items: &mut [&inplace::Paragraph]) {
    if items.len() < 2 {
        return;
    }
    // Spanning prose separates column bands. Isolate the whole band before
    // cutting columns, so a side box cannot interleave with the adjacent list.
    let width = items
        .iter()
        .map(|p| p.rect.right)
        .fold(f64::NEG_INFINITY, f64::max)
        - items
            .iter()
            .map(|p| p.rect.left)
            .fold(f64::INFINITY, f64::min);
    let spanning = |p: &inplace::Paragraph| p.rect.right - p.rect.left >= width * 0.7;
    if items.iter().filter(|p| spanning(p)).count() >= 2 && items.iter().any(|p| !spanning(p)) {
        items.sort_by(|a, b| {
            a.rect
                .top
                .total_cmp(&b.rect.top)
                .then(a.rect.left.total_cmp(&b.rect.left))
        });
        let mut start = 0;
        for i in 0..items.len() {
            if spanning(items[i]) {
                reading_order(&mut items[start..i]);
                start = i + 1;
            }
        }
        reading_order(&mut items[start..]);
        return;
    }
    for vertical in [true, false] {
        let start = |p: &inplace::Paragraph| if vertical { p.rect.left } else { p.rect.top };
        let end = |p: &inplace::Paragraph| {
            if vertical {
                p.rect.right
            } else {
                p.rect.bottom
            }
        };
        items.sort_by(|a, b| start(a).total_cmp(&start(b)));
        let mut edge = end(items[0]);
        let mut cut = None;
        let mut gap = 0.5;
        for (i, p) in items.iter().enumerate().skip(1) {
            if start(p) - edge > gap {
                gap = start(p) - edge;
                cut = Some(i);
            }
            edge = edge.max(end(p));
        }
        if let Some(i) = cut {
            let (left, right) = items.split_at_mut(i);
            reading_order(left);
            reading_order(right);
            return;
        }
    }
    items.sort_by(|a, b| {
        a.rect
            .top
            .total_cmp(&b.rect.top)
            .then(a.rect.left.total_cmp(&b.rect.left))
    });
}

#[cfg(test)]
mod tests {
    use super::*;
    fn paragraph(id: &str, left: f64, top: f64, right: f64, bottom: f64) -> inplace::Paragraph {
        inplace::Paragraph {
            id: id.into(),
            page: 0,
            rect: super::super::layout::Rect {
                left,
                top,
                right,
                bottom,
            },
            size: 12.0,
            line_height: 14.0,
            align: super::super::layout::Align::Left,
            lines: 1,
            text: id.into(),
            body: true,
            table: false,
            prose: true,
            figure: false,
            status: Status::Translate,
            source_lines: vec![id.into()],
            maths: vec![],
            line_rects: vec![],
        }
    }
    #[test]
    fn added_page_reading_order_places_heading_columns_then_footer() {
        let values = [
            paragraph("footer", 0., 100., 100., 110.),
            paragraph("right2", 60., 60., 100., 70.),
            paragraph("left2", 0., 60., 40., 70.),
            paragraph("heading", 0., 0., 100., 10.),
            paragraph("right1", 60., 30., 100., 40.),
            paragraph("left1", 0., 30., 40., 40.),
        ];
        let mut order: Vec<_> = values.iter().collect();
        reading_order(&mut order);
        assert_eq!(
            order.iter().map(|p| p.id.as_str()).collect::<Vec<_>>(),
            ["heading", "left1", "left2", "right1", "right2", "footer"]
        );
    }
    #[test]
    fn side_box_follows_the_whole_list_between_spanning_paragraphs() {
        let values = [
            paragraph("heading", 0., 0., 100., 10.),
            paragraph("item1", 0., 10.2, 40., 20.),
            paragraph("box1", 60., 12., 95., 22.),
            paragraph("item2", 0., 20.2, 40., 30.),
            paragraph("box2", 60., 22.2, 75., 32.),
            paragraph("item3", 0., 30.2, 40., 40.),
            paragraph("following", 0., 40.2, 100., 50.),
        ];
        let mut order: Vec<_> = values.iter().collect();
        reading_order(&mut order);
        assert_eq!(
            order.iter().map(|p| p.id.as_str()).collect::<Vec<_>>(),
            [
                "heading",
                "item1",
                "item2",
                "item3",
                "box1",
                "box2",
                "following"
            ]
        );
    }
    #[test]
    fn only_repeated_single_lines_qualify_for_consolidation() {
        let mut caption = paragraph("Table 1: Categories (Continued)", 30., 20., 400., 35.);
        let mut copies: Vec<_> = (0..12)
            .map(|page| {
                let mut p = caption.clone();
                p.page = page;
                p
            })
            .collect();
        assert!(!repeated_margin_line(&caption, &copies[..2], 8));
        assert!(!repeated_margin_line(&caption, &copies[..8], 48));
        assert!(repeated_margin_line(&caption, &copies, 48));
        copies.iter_mut().for_each(|p| p.lines = 2);
        caption.lines = 2;
        assert!(!repeated_margin_line(&caption, &copies, 48));
        copies.iter_mut().for_each(|p| {
            p.lines = 1;
            p.page = 0;
        });
        caption.lines = 1;
        assert!(!repeated_margin_line(&caption, &copies, 12));
    }
    #[test]
    fn equal_translations_ignore_only_whitespace_and_case() {
        assert!(same_text(" LAMP EX\n100-1 ", "lamp ex 100-1"));
        assert!(!same_text("LAMP EX 100-1", "LAMP EX 100-2"));
        assert!(!same_text("text™", "text"));
    }
    #[test]
    fn caught_panic_preserves_its_detail_for_the_app_log() {
        let error = guarded::<()>(|| panic!("synthetic layout panic detail")).unwrap_err();
        assert!(matches!(error, crate::FileError::PdfPanic(_)));
        assert!(error.to_string().contains("synthetic layout panic detail"));
    }
    #[test]
    fn panic_is_an_error_and_subsequent_pdfium_work_recovers() {
        let result: Result<(), crate::FileError> = guarded(|| {
            let (_guard, _) = super::super::pdfium()?;
            panic!("synthetic panic while PDFium lock is held")
        });
        assert!(result.is_err());
        // This unit test also runs without PDFium; the ignored integration test
        // checks the lock recovery with the real library.
    }
    #[test]
    #[ignore = "requires local PDFium"]
    fn poisoned_pdfium_lock_can_be_used_again() {
        let _: Result<(), crate::FileError> = guarded(|| {
            let (_guard, _) = super::super::pdfium()?;
            panic!("synthetic PDFium panic")
        });
        assert!(super::super::pdfium().is_ok());
    }
}
