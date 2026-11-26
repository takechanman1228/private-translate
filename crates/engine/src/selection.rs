//! Selection policy for model suggestions and translation.

use crate::models::CatalogEntry;
use crate::profiles::{FileFormat, ModelProfile};

#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
pub enum QualityTier {
    #[default]
    Standard,
    High,
}

#[derive(Debug, PartialEq, Eq)]
pub enum Reason {
    DownloadSuggested,
    ConsentRequired,
    ConsentRequiredWithoutAlternative,
    DownloadedModel,
    NoCompatibleModel,
    HighQualityMemoryShortfall,
}

pub struct Suggestion {
    pub entry: &'static CatalogEntry,
    pub needs_consent: bool,
}

pub struct Selection {
    pub usable: Option<&'static CatalogEntry>,
    pub suggestion: Option<Suggestion>,
    pub alternative: Option<&'static CatalogEntry>,
    pub reason: Reason,
    pub quality_reason: Option<Reason>,
}

#[derive(Clone, Copy)]
pub struct Request<'a> {
    pub source: Option<&'a str>,
    pub target: Option<&'a str>,
    /// None denotes text entered in the translation pane.
    pub format: Option<FileFormat>,
    pub system_memory: u64,
    pub quality: QualityTier,
    pub plamo_consent: bool,
    pub bidirectional: bool,
    pub selected: Option<&'a str>,
}

const PLAMO_2B: &str = "2b";
const PLAMO_8B: &str = "8b";
const HY: &str = crate::models::HY_MT2.pinned_id.unwrap();
const HY_7B: &str = crate::models::HY_MT2_7B.pinned_id.unwrap();
const INDEX: &str = crate::models::INDEX_TRANSLATE.pinned_id.unwrap();

/// The high-quality prefix is prepended to the standard order.
fn priority(
    source: Option<&str>,
    target: Option<&str>,
    bidirectional: bool,
) -> (&'static [&'static str], &'static [&'static str]) {
    if bidirectional {
        return (&[PLAMO_8B, HY_7B], &[PLAMO_2B, HY, INDEX]);
    }
    match (source, target) {
        (Some("ja"), Some("en")) => (&[PLAMO_8B, HY_7B], &[PLAMO_2B, INDEX, HY]),
        (Some("en"), Some("ja")) => (&[PLAMO_8B, HY_7B], &[PLAMO_2B, HY, INDEX]),
        _ if supports_direction(ModelProfile::HyMt2, source, target) => {
            (&[HY_7B, PLAMO_8B], &[HY, INDEX, PLAMO_2B])
        }
        _ => (&[PLAMO_8B], &[PLAMO_2B]),
    }
}

pub fn supports_direction(
    profile: ModelProfile,
    source: Option<&str>,
    target: Option<&str>,
) -> bool {
    if source.is_some() && source == target {
        return false;
    }
    if profile == ModelProfile::PlamoPfml {
        source.is_none_or(|code| pfml::find_language(code).is_some())
            && target
                .is_none_or(|code| code == pfml::JA_EASY || pfml::find_language(code).is_some())
    } else {
        profile
            .capabilities()
            .targets(source)
            .any(|code| Some(code) == target)
    }
}

/// Weights plus working memory at the full 32k context, including KV cache and compute buffers.
pub fn estimate_required(model_bytes: u64) -> u64 {
    model_bytes
        .saturating_add(model_bytes / 5)
        .saturating_add(384 << 20)
}

pub fn fits_memory_budget(model_bytes: u64, system_memory: u64) -> bool {
    estimate_required(model_bytes) <= system_memory / 2
}

pub fn select(
    request: Request<'_>,
    catalog: &[&'static CatalogEntry],
    downloaded: &[&str],
) -> Selection {
    let compatible = |entry: &&CatalogEntry| {
        supports_direction(entry.profile, request.source, request.target)
            && (!request.bidirectional
                || supports_direction(entry.profile, request.target, request.source))
            && request
                .format
                .is_none_or(|format| entry.capabilities().files.contains(&format))
    };
    let fits = |entry: &CatalogEntry| fits_memory_budget(entry.bytes, request.system_memory);
    let markdown = request.format == Some(FileFormat::Markdown);
    let document = request.format.is_some_and(FileFormat::is_document);
    let (high, standard) = if document {
        (&[PLAMO_8B][..], &[PLAMO_2B, HY_7B, HY, INDEX][..])
    } else {
        priority(request.source, request.target, request.bidirectional)
    };
    let needs_file_memory =
        |slot| document && matches!(slot, HY_7B | HY | INDEX) || markdown && slot == INDEX;
    let requires_fit = |slot| high.contains(&slot) || needs_file_memory(slot);
    let key = |entry: &'static CatalogEntry| {
        if entry.is_plamo() {
            entry.size
        } else {
            entry.id()
        }
    };
    let needs_consent = |entry: &CatalogEntry| entry.is_plamo() && !request.plamo_consent;
    let order: Vec<_> = high
        .iter()
        .copied()
        .filter(|_| request.quality == QualityTier::High)
        .chain(standard.iter().copied())
        .collect();
    let variant_priority = |entry: &CatalogEntry| {
        (
            request.selected != Some(entry.id()),
            crate::models::quant_rank(entry.quant),
        )
    };
    let candidates = order.iter().filter_map(|&slot| {
        // Markdown can use an installed Index model, without suggesting a download.
        if markdown && slot == INDEX && !downloaded.contains(&INDEX) {
            return None;
        }
        let entries = catalog
            .iter()
            .copied()
            .filter(compatible)
            .filter(|entry| key(entry) == slot);
        let installed = entries
            .clone()
            .filter(|entry| downloaded.contains(&entry.id()))
            .min_by_key(|entry| (requires_fit(slot) && !fits(entry), variant_priority(entry)));
        installed
            .or_else(|| {
                entries
                    .into_iter()
                    .find(|entry| !entry.is_plamo() || entry.quant == "Q4_K_M")
            })
            .filter(|entry| !requires_fit(slot) || fits(entry))
    });
    let first = candidates.clone().next();
    let mut installed_order = order.clone();
    if request.quality == QualityTier::Standard {
        installed_order.extend_from_slice(high);
    }
    let usable = catalog
        .iter()
        .copied()
        .filter(compatible)
        .filter(|entry| !needs_consent(entry) && downloaded.contains(&entry.id()))
        .filter(|entry| !needs_file_memory(entry.id()) || fits(entry))
        .min_by_key(|entry| {
            (
                document && !entry.is_plamo(),
                (entry.is_plamo() && entry.params_billion() >= 8.0 || entry.id() == HY_7B)
                    && !fits(entry),
                installed_order
                    .iter()
                    .position(|&slot| slot == key(entry))
                    .unwrap_or(installed_order.len()),
                // The table decides between sizes; the selection breaks ties within a size.
                if installed_order.contains(&key(entry)) {
                    0
                } else {
                    entry.params_billion().to_bits()
                },
                variant_priority(entry),
            )
        });
    let high_quality_memory_shortfall = request.quality == QualityTier::High
        && first.is_some_and(|entry| !high.contains(&key(entry)))
        && catalog
            .iter()
            .copied()
            .filter(compatible)
            .any(|entry| high.contains(&key(entry)) && !fits(entry))
        && usable.is_none_or(|entry| !high.contains(&key(entry)));
    let suggestion = first
        .filter(|entry| Some(key(entry)) != usable.map(key))
        .map(|entry| Suggestion {
            entry,
            needs_consent: needs_consent(entry),
        });
    let alternative = if suggestion.as_ref().is_some_and(|s| s.needs_consent) {
        candidates.into_iter().find(|entry| !entry.is_plamo())
    } else {
        None
    };
    let reason = match &suggestion {
        Some(s) if s.needs_consent && alternative.is_none() && usable.is_none() => {
            Reason::ConsentRequiredWithoutAlternative
        }
        Some(s) if s.needs_consent => Reason::ConsentRequired,
        Some(_) => Reason::DownloadSuggested,
        None if usable.is_some() => Reason::DownloadedModel,
        None => Reason::NoCompatibleModel,
    };
    Selection {
        usable,
        suggestion,
        alternative,
        reason,
        quality_reason: high_quality_memory_shortfall.then_some(Reason::HighQualityMemoryShortfall),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    const PLAMO_2B: &str = "plamo-3-translate-2b-Q4_K_M";
    const PLAMO_8B: &str = "plamo-3-translate-8b-Q4_K_M";
    use crate::models::{BUILTIN, HY_MT2, HY_MT2_7B, INDEX_TRANSLATE};

    fn pick(
        (source, target): (&str, &str),
        format: Option<FileFormat>,
        quality: QualityTier,
        system_memory: u64,
        plamo_consent: bool,
        downloaded: &[&str],
    ) -> Selection {
        let mut catalog: Vec<_> = BUILTIN.iter().collect();
        catalog.extend([&HY_MT2, &HY_MT2_7B, &INDEX_TRANSLATE]);
        select(
            Request {
                source: Some(source),
                target: Some(target),
                format,
                quality,
                system_memory,
                plamo_consent,
                bidirectional: false,
                selected: None,
            },
            &catalog,
            downloaded,
        )
    }

    #[test]
    fn downloaded_variants_follow_size_selection_precision_and_memory() {
        let entries: Vec<_> = BUILTIN.iter().collect();
        let variant = |size, quant| {
            entries
                .iter()
                .find(|entry| entry.size == size && entry.quant == quant)
                .unwrap()
                .id()
        };
        let q40 = variant("2b", "Q4_0");
        let q2 = variant("2b", "Q2_K");
        let eight = variant("8b", "Q3_K_M");
        let small = variant("0.8b", "Q4_K_M");
        let large = variant("31b", "Q4_K_M");
        let mut request = Request {
            source: Some("ja"),
            target: Some("en"),
            format: Some(FileFormat::Pdf),
            system_memory: u64::MAX,
            quality: QualityTier::High,
            plamo_consent: true,
            bidirectional: true,
            selected: None,
        };
        let downloaded = [q2, q40, eight, small, large];
        assert_eq!(
            select(request, &entries, &downloaded).usable.unwrap().id(),
            eight
        );
        request.system_memory = 0;
        let result = select(request, &entries, &downloaded);
        assert_eq!(result.usable.unwrap().id(), q40);
        assert_eq!(
            result.quality_reason,
            Some(Reason::HighQualityMemoryShortfall)
        );
        request.selected = Some(q2);
        assert_eq!(
            select(request, &entries, &downloaded).usable.unwrap().id(),
            q2
        );
        assert_eq!(
            select(request, &entries, &[small, large])
                .usable
                .unwrap()
                .id(),
            small
        );
        assert_eq!(
            select(request, &entries, &[large]).usable.unwrap().id(),
            large
        );
        request.system_memory = u64::MAX;
        assert_eq!(
            select(request, &entries, &[large]).usable.unwrap().id(),
            large
        );
    }

    #[test]
    fn bidirectional_setup_prefers_hy_mt2_as_alternative() {
        let mut entries: Vec<_> = BUILTIN.iter().collect();
        entries.extend([&HY_MT2, &HY_MT2_7B, &INDEX_TRANSLATE]);
        let result = select(
            Request {
                source: Some("ja"),
                target: Some("en"),
                format: None,
                system_memory: u64::MAX,
                quality: QualityTier::Standard,
                plamo_consent: false,
                bidirectional: true,
                selected: None,
            },
            &entries,
            &[PLAMO_2B],
        );
        assert!(result.usable.is_none());
        assert_eq!(result.suggestion.unwrap().entry.id(), PLAMO_2B);
        assert_eq!(result.alternative.unwrap().id(), HY);
    }

    #[test]
    fn downloaded_large_models_remain_usable_and_follow_the_other_tier() {
        for (pair, format, installed, expected) in [
            (("ja", "en"), None, &[PLAMO_8B][..], PLAMO_8B),
            (("en", "fr"), None, &[HY_7B][..], HY_7B),
            (
                ("en", "ja"),
                Some(FileFormat::Pdf),
                &[HY, PLAMO_8B][..],
                PLAMO_8B,
            ),
        ] {
            let result = pick(
                pair,
                format,
                QualityTier::Standard,
                8 << 30,
                true,
                installed,
            );
            assert_eq!(result.usable.unwrap().id(), expected);
        }
        let result = pick(
            ("ja", "en"),
            None,
            QualityTier::Standard,
            64 << 30,
            true,
            &[HY_7B, PLAMO_8B],
        );
        assert_eq!(result.usable.unwrap().id(), PLAMO_8B);
    }

    #[test]
    fn downloaded_quantization_suppresses_suggestion_for_the_same_size() {
        let installed = "plamo-3-translate-2b-Q4_0";
        let result = pick(
            ("ja", "en"),
            None,
            QualityTier::Standard,
            8 << 30,
            true,
            &[installed],
        );
        assert_eq!(result.usable.unwrap().id(), installed);
        assert!(result.suggestion.is_none());
        let unconsented = pick(
            ("ja", "en"),
            None,
            QualityTier::Standard,
            8 << 30,
            false,
            &[installed],
        );
        let suggestion = unconsented.suggestion.unwrap();
        assert!(suggestion.needs_consent);
        assert_eq!(suggestion.entry.id(), installed);
    }

    #[test]
    fn ranking_covers_each_direction_group_and_quality() {
        for (pair, standard, high, alternate) in [
            (("ja", "en"), PLAMO_2B, PLAMO_8B, Some(INDEX)),
            (("en", "ja"), PLAMO_2B, PLAMO_8B, Some(HY)),
            (("en", "fr"), HY, HY_7B, None),
            (("de", "en"), HY, HY_7B, None),
            (("ja", "ko"), PLAMO_2B, PLAMO_8B, None),
        ] {
            for (tier, expected) in [(QualityTier::Standard, standard), (QualityTier::High, high)] {
                let result = pick(pair, None, tier, u64::MAX, true, &[]);
                assert_eq!(
                    result.suggestion.unwrap().entry.id(),
                    expected,
                    "{pair:?} {tier:?}"
                );
                let result = pick(pair, None, tier, 0, true, &[]);
                assert_eq!(result.suggestion.unwrap().entry.id(), standard);
                let result = pick(pair, None, tier, u64::MAX, false, &[]);
                let expected_alt = if tier == QualityTier::High {
                    alternate.map(|_| HY_7B)
                } else {
                    alternate
                };
                assert_eq!(result.alternative.map(CatalogEntry::id), expected_alt);
                assert_eq!(
                    result.suggestion.unwrap().needs_consent,
                    expected == PLAMO_2B || expected == PLAMO_8B
                );
            }
        }
    }

    #[test]
    fn new_english_pairs_prefer_hy_mt2_and_filter_out_index() {
        for language in ["zh-hans", "es", "ko"] {
            for pair in [("en", language), (language, "en")] {
                for (tier, hy, plamo) in [
                    (QualityTier::Standard, HY, PLAMO_2B),
                    (QualityTier::High, HY_7B, PLAMO_8B),
                ] {
                    for format in [None, Some(FileFormat::Txt), Some(FileFormat::Markdown)] {
                        let result = pick(pair, format, tier, u64::MAX, false, &[INDEX]);
                        let suggestion = result.suggestion.unwrap();
                        assert_eq!(suggestion.entry.id(), hy);
                        assert!(!suggestion.needs_consent);
                        assert!(result.usable.is_none());

                        let result = pick(
                            pair,
                            format,
                            tier,
                            u64::MAX,
                            true,
                            &[INDEX, HY, HY_7B, plamo],
                        );
                        assert_eq!(result.usable.unwrap().id(), hy);
                        let fallback = pick(pair, format, tier, u64::MAX, true, &[INDEX, plamo]);
                        assert_eq!(fallback.usable.unwrap().id(), plamo);
                    }
                    for format in [FileFormat::Word, FileFormat::PowerPoint, FileFormat::Pdf] {
                        let result = pick(pair, Some(format), tier, u64::MAX, false, &[HY, HY_7B]);
                        assert_eq!(result.usable.map(CatalogEntry::id), Some(HY_7B));
                        let suggestion = result.suggestion.unwrap();
                        assert_eq!(suggestion.entry.id(), plamo);
                        assert!(suggestion.needs_consent);
                    }
                }
                assert!(!supports_direction(
                    ModelProfile::HyMt2,
                    Some(language),
                    Some("ja")
                ));
            }
        }
    }

    #[test]
    fn markdown_and_documents_exclude_index_when_memory_is_insufficient() {
        let pair = ("ja", "en");
        let tier = QualityTier::Standard;
        let result = pick(
            pair,
            Some(FileFormat::Markdown),
            tier,
            0,
            false,
            &[INDEX, PLAMO_2B, HY],
        );
        assert_eq!(result.reason, Reason::ConsentRequired);
        assert_eq!(result.usable.unwrap().id(), HY);
        assert_eq!(result.alternative.unwrap().id(), HY);
        for format in [FileFormat::Word, FileFormat::PowerPoint, FileFormat::Pdf] {
            let result = pick(pair, Some(format), tier, 0, false, &[HY, PLAMO_2B]);
            assert!(result.usable.is_none() && result.alternative.is_none());
            assert!(result.suggestion.unwrap().needs_consent);
            let result = pick(pair, Some(format), tier, 0, true, &[HY, PLAMO_2B]);
            assert_eq!(result.usable.unwrap().id(), PLAMO_2B);
            assert!(result.suggestion.is_none());
        }
        let result = pick(pair, Some(FileFormat::Txt), tier, 0, false, &[HY, INDEX]);
        assert_eq!(result.usable.unwrap().id(), INDEX);
    }

    #[test]
    fn index_markdown_uses_text_priority_with_a_memory_requirement() {
        let tier = QualityTier::Standard;
        let pair = ("ja", "en");
        let markdown = Some(FileFormat::Markdown);
        let usable = |format, gib: u64, consent, installed: &[&str]| {
            pick(pair, format, tier, gib << 30, consent, installed)
                .usable
                .map(CatalogEntry::id)
        };
        for (memory, consent, installed, expected) in [
            (16, true, &[PLAMO_2B, HY_7B, HY, INDEX][..], PLAMO_2B),
            (16, false, &[PLAMO_2B, HY_7B, HY, INDEX][..], INDEX),
            (8, false, &[HY_7B, HY, INDEX][..], INDEX),
            (8, false, &[HY_7B, INDEX][..], INDEX),
            (8, false, &[INDEX][..], INDEX),
            (16, false, &[HY_7B, HY][..], HY),
        ] {
            assert_eq!(usable(markdown, memory, consent, installed), Some(expected));
        }
        for format in [None, Some(FileFormat::Txt)] {
            assert_eq!(usable(format, 16, false, &[HY, INDEX]), Some(INDEX));
        }
        assert_eq!(
            pick(("en", "ja"), markdown, tier, 8 << 30, false, &[INDEX, HY])
                .usable
                .map(CatalogEntry::id),
            Some(HY)
        );
        assert_eq!(usable(markdown, 1, false, &[INDEX]), None);
        assert_eq!(
            pick(pair, markdown, tier, 8 << 30, false, &[])
                .alternative
                .map(CatalogEntry::id),
            Some(HY)
        );
    }

    #[test]
    fn installing_index_keeps_other_markdown_models_in_text_order() {
        for installed in [&[PLAMO_2B, HY][..], &[HY_7B, HY], &[PLAMO_2B, HY_7B, HY]] {
            let mut with_index = installed.to_vec();
            with_index.push(INDEX);
            for tier in [QualityTier::Standard, QualityTier::High] {
                let usable = |downloaded| {
                    pick(
                        ("de", "en"),
                        Some(FileFormat::Markdown),
                        tier,
                        16 << 30,
                        true,
                        downloaded,
                    )
                    .usable
                    .map(CatalogEntry::id)
                };
                let expected = if tier == QualityTier::High && installed.contains(&HY_7B) {
                    HY_7B
                } else {
                    HY
                };
                assert_eq!(usable(installed), Some(expected));
                assert_eq!(usable(&with_index), Some(expected));
            }
        }
    }

    #[test]
    fn portuguese_excludes_plamo_and_unsupported_pairs_yield_no_selection() {
        let tier = QualityTier::High;
        let pair = ("pt", "en");
        let result = pick(pair, None, tier, u64::MAX, true, &[PLAMO_8B, INDEX]);
        assert_eq!(result.usable.unwrap().id(), INDEX);
        assert_eq!(result.suggestion.unwrap().entry.id(), HY_7B);
        for (pair, format) in [(("pt", "ja"), None), (("en", "en"), None)] {
            let result = pick(pair, format, tier, u64::MAX, true, &[PLAMO_2B, HY]);
            assert_eq!(result.reason, Reason::NoCompatibleModel);
            assert!(
                result.usable.is_none()
                    && result.suggestion.is_none()
                    && result.alternative.is_none()
            );
        }
    }

    #[test]
    fn documents_prefer_consented_plamo_and_require_a_fitting_7b_with_a_known_direction() {
        let boundary = 2 * estimate_required(HY_MT2_7B.bytes);
        let installed = [PLAMO_2B, HY_7B, HY, INDEX];
        for format in [FileFormat::Word, FileFormat::PowerPoint, FileFormat::Pdf] {
            for quality in [QualityTier::Standard, QualityTier::High] {
                let mut request = Request {
                    source: Some("en"),
                    target: Some("fr"),
                    format: Some(format),
                    system_memory: boundary,
                    quality,
                    plamo_consent: true,
                    bidirectional: false,
                    selected: None,
                };
                let catalog = [
                    &HY_MT2_7B,
                    &HY_MT2,
                    &INDEX_TRANSLATE,
                    crate::models::find_entry(PLAMO_2B).unwrap(),
                ];
                let usable = |request, installed: &[&str]| {
                    select(request, &catalog, installed)
                        .usable
                        .map(CatalogEntry::id)
                };
                assert_eq!(usable(request, &installed), Some(PLAMO_2B));
                assert_eq!(usable(request, &installed[1..]), Some(HY_7B));
                request.plamo_consent = false;
                assert_eq!(usable(request, &installed), Some(HY_7B));
                assert_eq!(usable(request, &installed[2..]), Some(HY));
                assert_eq!(
                    usable(request, &[INDEX]),
                    (format == FileFormat::Word).then_some(INDEX)
                );
                assert_eq!(
                    usable(
                        Request {
                            system_memory: boundary - 1,
                            ..request
                        },
                        &installed
                    ),
                    Some(HY)
                );
                for (gib, consent, expected) in [
                    (8, true, PLAMO_2B),
                    (8, false, HY),
                    (16, true, PLAMO_2B),
                    (16, false, HY_7B),
                ] {
                    let request = Request {
                        system_memory: gib << 30,
                        plamo_consent: consent,
                        ..request
                    };
                    assert_eq!(usable(request, &installed), Some(expected));
                    assert_eq!(
                        usable(request, &[PLAMO_2B, HY, INDEX]),
                        Some(if consent { PLAMO_2B } else { HY })
                    );
                }
                assert_eq!(
                    usable(
                        Request {
                            source: None,
                            ..request
                        },
                        &installed
                    ),
                    None
                );
                assert_eq!(
                    usable(
                        Request {
                            source: Some("ja"),
                            ..request
                        },
                        &installed
                    ),
                    None
                );
                assert_eq!(
                    usable(
                        Request {
                            source: Some("pt"),
                            target: Some("en"),
                            plamo_consent: true,
                            ..request
                        },
                        &installed
                    ),
                    Some(HY_7B)
                );
            }
        }
    }

    #[test]
    fn pdf_suggests_the_larger_hy_model_whenever_it_fits() {
        let request = Request {
            source: Some("en"),
            target: Some("pt"),
            format: Some(FileFormat::Pdf),
            system_memory: 8 << 30,
            quality: QualityTier::Standard,
            plamo_consent: false,
            bidirectional: false,
            selected: None,
        };
        let catalog = [&HY_MT2, &HY_MT2_7B];
        assert_eq!(
            select(request, &catalog, &[])
                .suggestion
                .unwrap()
                .entry
                .id(),
            HY
        );
        let larger_mac = Request {
            system_memory: 16 << 30,
            ..request
        };
        assert_eq!(
            select(larger_mac, &catalog, &[HY])
                .suggestion
                .unwrap()
                .entry
                .id(),
            HY_7B
        );
    }

    #[test]
    fn high_quality_respects_inclusive_half_memory_boundary() {
        let boundary = 2 * estimate_required(HY_MT2_7B.bytes);
        for (memory, expected) in [(boundary, HY_7B), (boundary - 1, HY)] {
            let result = pick(("en", "pt"), None, QualityTier::High, memory, true, &[]);
            assert_eq!(result.suggestion.unwrap().entry.id(), expected);
        }
    }
}
