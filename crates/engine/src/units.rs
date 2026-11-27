//! File units share the document translator while letting callers protect tagged content.

use std::sync::Arc;

pub use crate::document::translate_units;

pub type UnitObserver = Arc<dyn Fn(usize, &str) + Send + Sync>;

pub type Validator = Arc<dyn Fn(usize, &str) -> bool + Send + Sync>;

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum UnitResult {
    Complete {
        text: String,
        retried: bool,
    },
    /// Translation of a tagged unit without inline formatting anchors.
    Simplified {
        text: String,
    },
    Cut,
    TooLong,
    Invalid,
    PromptLeak,
}

pub struct Unit {
    pub text: String,
    /// Guard tags or inline elements against being split during recovery.
    pub indivisible: bool,
}

pub struct AnchorFallback {
    /// Recover untagged source only for units that can be safely reinserted.
    pub source: Arc<dyn Fn(usize) -> Option<String> + Send + Sync>,
    /// Validate that an untagged result can actually be applied to the original unit.
    pub validate: Validator,
}

#[derive(Default)]
pub struct UnitTuning {
    /// Contexts shared by groups of units, with an optional context index per unit.
    pub contexts: Vec<String>,
    pub context_by_unit: Vec<usize>,
    /// A different group starts a fresh translation history.
    pub history_by_unit: Vec<usize>,
    pub reuse_translations: bool,
    /// Use the text translator's early heading-leak recovery in plain Hy-MT2 1.8B units.
    pub recover_prompt_leaks: bool,
    /// Keep Office anchors and retry invalid tagged candidates on supported models.
    pub preserve_office_anchors: bool,
    pub anchor_fallback: Option<AnchorFallback>,
    /// Observe complete candidates, including empty and invalid ones, before validation.
    pub observe_candidate: Option<UnitObserver>,
}

pub struct UnitRequest {
    pub units: Vec<Unit>,
    pub tuning: UnitTuning,
    pub max_unit_tokens: usize,
    pub max_history_tokens: usize,
    pub validate: Validator,
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{Generation, Result, StopReason, TranslateStats, TranslationBackend};
    use pfml::LangCode;
    use pfml::{Prompt, TranslationOptions};
    use std::time::Duration;

    #[derive(Default)]
    struct Fake {
        history: Vec<Vec<String>>,
        calls: Vec<String>,
        contexts: Vec<String>,
    }

    impl TranslationBackend for Fake {
        fn translate(
            &mut self,
            prompt: &Prompt<'_>,
            _: &Generation,
            sink: &mut dyn FnMut(&str),
            _: &dyn Fn() -> bool,
        ) -> Result<TranslateStats> {
            self.calls.push(prompt.text.into());
            self.contexts.push(prompt.options.context.clone());
            self.history.push(
                prompt
                    .history
                    .iter()
                    .map(|p| p.translation.clone())
                    .collect(),
            );
            sink(prompt.text);
            Ok(TranslateStats {
                prompt_tokens: 10,
                generated_tokens: prompt.text.len(),
                prompt_duration: Duration::ZERO,
                generation_duration: Duration::ZERO,
                stop_reason: match prompt.text {
                    "cut" => StopReason::LengthLimit,
                    "cancel" => StopReason::Cancelled,
                    _ => StopReason::Finished,
                },
                retries: 0,
                cut: 0,
                prompt_leaks: 0,
            })
        }
    }

    fn tagged(texts: &[&str]) -> Vec<Unit> {
        texts
            .iter()
            .map(|text| Unit {
                text: (*text).into(),
                indivisible: true,
            })
            .collect()
    }

    #[test]
    fn file_context_and_history_stay_in_scope_and_repeated_units_reuse_valid_output() {
        let mut backend = Fake::default();
        let request = UnitRequest {
            units: tagged(&["heading", "line", "line2", "heading", "line3"]),
            tuning: UnitTuning {
                contexts: vec!["first slide".into(), "second slide".into()],
                context_by_unit: vec![0, 0, 1, 1, 1],
                history_by_unit: vec![1, 1, 2, 2, 3],
                reuse_translations: true,
                ..Default::default()
            },
            max_unit_tokens: 100,
            max_history_tokens: 100,

            validate: Arc::new(|_, _| true),
        };
        let mut output = Vec::new();
        translate_units(
            &mut backend,
            &str::len,
            4096,
            &TranslationOptions::new(LangCode::new("en").unwrap()),
            &request,
            &mut |_, result| output.push(result),
            &|| false,
        )
        .unwrap();
        assert_eq!(backend.calls, ["heading", "line", "line2", "line3"]);
        assert_eq!(
            backend.contexts,
            ["first slide", "first slide", "second slide", "second slide"]
        );
        assert_eq!(
            backend.history,
            [vec![], vec!["heading".to_owned()], vec![], vec![]]
        );
        assert_eq!(output[0], output[3]);
        assert_eq!(output.len(), 5);
    }

    #[test]
    fn failed_units_are_not_committed_or_used_as_history() {
        let mut backend = Fake::default();
        let request = UnitRequest {
            tuning: UnitTuning::default(),
            units: tagged(&["good", "bad", "cut", "last"]),
            max_unit_tokens: 100,
            max_history_tokens: 1000,

            validate: Arc::new(|_, text| text != "bad"),
        };
        let mut results = Vec::new();
        let stats = translate_units(
            &mut backend,
            &str::len,
            4096,
            &TranslationOptions::new(LangCode::new("en").unwrap()),
            &request,
            &mut |i, r| results.push((i, r)),
            &|| false,
        )
        .unwrap();
        assert_eq!(
            results,
            vec![
                (
                    0,
                    UnitResult::Complete {
                        text: "good".into(),
                        retried: false
                    }
                ),
                (1, UnitResult::Invalid),
                (2, UnitResult::Cut),
                (
                    3,
                    UnitResult::Complete {
                        text: "last".into(),
                        retried: false
                    }
                )
            ]
        );
        assert_eq!(backend.history.last().unwrap(), &["good"]);
        assert_eq!(stats.cut, 1);
        assert_eq!(stats.retries, 0);
    }

    #[test]
    fn observes_empty_candidate_without_passing_it_to_the_validator() {
        use std::sync::Mutex;

        let observed = Arc::new(Mutex::new(Vec::new()));
        let observer = Arc::clone(&observed);
        let request = UnitRequest {
            tuning: UnitTuning {
                observe_candidate: Some(Arc::new(move |i, text| {
                    observer.lock().unwrap().push((i, text.is_empty()));
                })),
                ..Default::default()
            },
            units: tagged(&["x"]),
            max_unit_tokens: 100,
            max_history_tokens: 0,
            validate: Arc::new(|_, _| panic!("empty text must bypass the validator")),
        };
        struct Empty;
        impl TranslationBackend for Empty {
            fn translate(
                &mut self,
                _: &Prompt<'_>,
                _: &Generation,
                _: &mut dyn FnMut(&str),
                _: &dyn Fn() -> bool,
            ) -> Result<TranslateStats> {
                Ok(TranslateStats {
                    stop_reason: StopReason::Finished,
                    ..TranslateStats::cancelled()
                })
            }
        }
        let mut results = Vec::new();
        translate_units(
            &mut Empty,
            &str::len,
            4096,
            &TranslationOptions::new(LangCode::new("en").unwrap()),
            &request,
            &mut |_, result| results.push(result),
            &|| false,
        )
        .unwrap();
        assert_eq!(*observed.lock().unwrap(), [(0, true)]);
        assert_eq!(results, [UnitResult::Invalid]);
    }

    #[test]
    fn oversized_unit_is_not_split_and_later_unit_runs() {
        let mut backend = Fake::default();
        let request = UnitRequest {
            tuning: UnitTuning::default(),
            units: tagged(&["<g1>do not split me</g1>", "ok"]),
            max_unit_tokens: 5,
            max_history_tokens: 100,

            validate: Arc::new(|_, _| true),
        };
        let mut results = Vec::new();
        translate_units(
            &mut backend,
            &str::len,
            4096,
            &TranslationOptions::new(LangCode::new("en").unwrap()),
            &request,
            &mut |_, r| results.push(r),
            &|| false,
        )
        .unwrap();
        assert_eq!(results[0], UnitResult::TooLong);
        assert_eq!(backend.calls, ["ok"]);
    }

    #[test]
    fn rejects_plain_unit_without_leaving_its_chunks_in_history() {
        let mut backend = Fake::default();
        let request = UnitRequest {
            tuning: UnitTuning::default(),
            units: ["a\n\nb", "c"]
                .into_iter()
                .map(|text| Unit {
                    text: text.into(),
                    indivisible: false,
                })
                .collect(),
            max_unit_tokens: 1,
            max_history_tokens: 100,

            validate: Arc::new(|i, _| i != 0),
        };
        let mut results = Vec::new();
        translate_units(
            &mut backend,
            &str::len,
            4096,
            &TranslationOptions::new(LangCode::new("en").unwrap()),
            &request,
            &mut |_, result| results.push(result),
            &|| false,
        )
        .unwrap();
        assert_eq!(backend.calls, ["a", "b", "c"]);
        assert!(backend.history.last().unwrap().is_empty());
        assert_eq!(results[0], UnitResult::Invalid);
    }

    #[test]
    fn cancellation_does_not_publish_a_unit() {
        let mut backend = Fake::default();
        let request = UnitRequest {
            tuning: UnitTuning::default(),
            units: tagged(&["hello"]),
            max_unit_tokens: 100,
            max_history_tokens: 100,

            validate: Arc::new(|_, _| true),
        };
        let stats = translate_units(
            &mut backend,
            &str::len,
            4096,
            &TranslationOptions::new(LangCode::new("en").unwrap()),
            &request,
            &mut |_, _| panic!("cancelled unit"),
            &|| true,
        )
        .unwrap();
        assert_eq!(stats.stop_reason, StopReason::Cancelled);
        assert!(backend.calls.is_empty());
    }

    #[test]
    fn cancellation_during_generation_discards_partial_text_and_stops_batch() {
        let mut backend = Fake::default();
        let request = UnitRequest {
            tuning: UnitTuning::default(),
            units: tagged(&["good", "cancel", "later"]),
            max_unit_tokens: 100,
            max_history_tokens: 100,

            validate: Arc::new(|_, _| true),
        };
        let mut published = Vec::new();
        let stats = translate_units(
            &mut backend,
            &str::len,
            4096,
            &TranslationOptions::new(LangCode::new("en").unwrap()),
            &request,
            &mut |i, result| published.push((i, result)),
            &|| false,
        )
        .unwrap();
        assert_eq!(stats.stop_reason, StopReason::Cancelled);
        assert_eq!(backend.calls, ["good", "cancel"]);
        assert_eq!(
            published,
            [(
                0,
                UnitResult::Complete {
                    text: "good".into(),
                    retried: false
                }
            )]
        );
    }

    struct Scripted<'a> {
        hy_mt2_1_8b: bool,
        stopped_early: usize,
        profile: crate::profiles::ModelProfile,
        replies: std::collections::VecDeque<(&'a str, StopReason)>,
        calls: Vec<(String, bool)>,
    }

    impl<'a> Scripted<'a> {
        fn finished(profile: crate::profiles::ModelProfile, replies: &[&'a str]) -> Self {
            Self {
                profile,
                hy_mt2_1_8b: false,
                stopped_early: 0,
                replies: replies
                    .iter()
                    .map(|&text| (text, StopReason::Finished))
                    .collect(),
                calls: Vec::new(),
            }
        }
    }

    impl TranslationBackend for Scripted<'_> {
        fn profile(&self) -> crate::profiles::ModelProfile {
            self.profile
        }

        fn is_hy_mt2_1_8b(&self) -> bool {
            self.hy_mt2_1_8b
        }

        fn translate(
            &mut self,
            prompt: &Prompt<'_>,
            generation: &Generation,
            sink: &mut dyn FnMut(&str),
            cancelled: &dyn Fn() -> bool,
        ) -> Result<TranslateStats> {
            self.calls
                .push((prompt.text.into(), generation.penalize_repetition));
            let (text, stop_reason) = self.replies.pop_front().expect("unexpected retry");
            // Separate pieces ensure the guard recognizes a header even across tokens.
            for ch in text.chars() {
                sink(ch.encode_utf8(&mut [0; 4]));
                if cancelled() {
                    self.stopped_early += 1;
                    return Ok(TranslateStats::cancelled());
                }
            }
            Ok(TranslateStats {
                stop_reason,
                generated_tokens: text.len(),
                ..TranslateStats::cancelled()
            })
        }
    }

    const TAGGED_SOURCE: &str = r#"A <a i="1">sample</a> text."#;
    const TAGGED_TARGET: &str = r#"<a i="1">Exemple</a> de texte."#;

    fn anchor_request(fallback_source: Option<&str>) -> UnitRequest {
        UnitRequest {
            units: tagged(&[TAGGED_SOURCE]),
            tuning: UnitTuning {
                preserve_office_anchors: true,
                anchor_fallback: Some(AnchorFallback {
                    source: {
                        let fallback_source = fallback_source.map(str::to_owned);
                        Arc::new(move |_| fallback_source.clone())
                    },
                    validate: Arc::new(|_, text| text == "Exemple de texte."),
                }),
                ..Default::default()
            },
            max_unit_tokens: 100,
            max_history_tokens: 0,
            validate: Arc::new(|_, text| text.contains(r#"<a i="1">"#) && text.contains("</a>")),
        }
    }

    fn run_units(
        backend: &mut Scripted<'_>,
        request: &UnitRequest,
    ) -> (Vec<UnitResult>, TranslateStats) {
        let mut results = Vec::new();
        let mut options = TranslationOptions::new(LangCode::new("fr").unwrap());
        options.source = Some(LangCode::new("en").unwrap());
        let stats = translate_units(
            backend,
            &str::len,
            4096,
            &options,
            request,
            &mut |_, result| results.push(result),
            &|| false,
        )
        .unwrap();
        assert!(backend.replies.is_empty());
        (results, stats)
    }

    #[test]
    fn hy_mt2_rescues_a_broken_anchor_or_leak_with_the_retry_sampler() {
        use crate::profiles::ModelProfile::HyMt2;
        for first in ["Exemple de texte.", "[Source Text]"] {
            let mut backend = Scripted::finished(HyMt2, &[first, TAGGED_TARGET]);
            let (results, stats) = run_units(&mut backend, &anchor_request(Some("A sample text.")));
            assert_eq!(
                results,
                [UnitResult::Complete {
                    text: TAGGED_TARGET.into(),
                    retried: true
                }]
            );
            assert_eq!(
                backend.calls,
                [(TAGGED_SOURCE.into(), false), (TAGGED_SOURCE.into(), true)]
            );
            assert_eq!(stats.retries, 1);
            assert_eq!(stats.prompt_leaks, usize::from(first == "[Source Text]"));
        }
    }

    #[test]
    fn hy_mt2_keeps_the_original_markdown_unit_when_anchors_are_invalid() {
        use crate::profiles::ModelProfile::HyMt2;
        let mut request = anchor_request(None);
        request.tuning = UnitTuning::default();
        let mut backend = Scripted::finished(HyMt2, &["Exemple de texte."]);
        let (results, stats) = run_units(&mut backend, &request);
        assert_eq!(results, [UnitResult::Invalid]);
        assert_eq!(stats.retries, 0);
        assert_eq!(backend.calls, [(TAGGED_SOURCE.into(), false)]);
    }

    #[test]
    fn hy_mt2_falls_back_only_if_the_untagged_result_is_accepted() {
        use crate::profiles::ModelProfile::HyMt2;
        for (source, last, expected) in [
            (
                Some("A sample text."),
                Some("Exemple de texte."),
                UnitResult::Simplified {
                    text: "Exemple de texte.".into(),
                },
            ),
            (Some("A sample text."), Some(""), UnitResult::Invalid),
            (None, None, UnitResult::Invalid),
        ] {
            let mut replies = vec!["bad tags", "still bad tags"];
            replies.extend(last);
            let mut backend = Scripted::finished(HyMt2, &replies);
            let (results, _) = run_units(&mut backend, &anchor_request(source));
            assert_eq!(results, [expected]);
            let mut expected_calls =
                vec![(TAGGED_SOURCE.into(), false), (TAGGED_SOURCE.into(), true)];
            expected_calls.extend(source.map(|text| (text.into(), false)));
            assert_eq!(backend.calls, expected_calls);
        }
    }

    #[test]
    fn hy_mt2_never_publishes_a_leaked_fallback_or_a_cancelled_retry() {
        use crate::profiles::ModelProfile::HyMt2;
        let request = anchor_request(Some("A sample text."));
        let mut backend = Scripted::finished(
            HyMt2,
            &[
                "invalid",
                "invalid",
                "[Source Text]",
                "Output only the translation.",
            ],
        );
        let (results, stats) = run_units(&mut backend, &request);
        assert_eq!(results, [UnitResult::Invalid]);
        assert_eq!(stats.prompt_leaks, 2);
        assert_eq!(backend.calls.last(), Some(&("A sample text.".into(), true)));

        let mut backend = Scripted::finished(HyMt2, &[]);
        backend.replies = [
            ("invalid", StopReason::Finished),
            ("partial", StopReason::Cancelled),
        ]
        .into();
        let (results, stats) = run_units(&mut backend, &request);
        assert!(results.is_empty());
        assert_eq!(stats.stop_reason, StopReason::Cancelled);
    }

    #[test]
    fn office_leak_recovery_keeps_anchors_whole_and_rejects_leaked_fallbacks() {
        use crate::profiles::ModelProfile::{HyMt2, PlamoPfml};

        let plain = "A sample text.";
        let mut request = anchor_request(Some(plain));
        request.tuning.recover_prompt_leaks = true;
        // Accept any fallback in this test so only the engine's leak check can reject it.
        request.tuning.anchor_fallback.as_mut().unwrap().validate = Arc::new(|_, _| true);
        let tagged_leak = r#"<a i="1">[Source Text]</a> Exemple."#;
        let late_leak = "Exemple. [Source Text]";
        for small in [false, true] {
            let mut backend = Scripted::finished(
                HyMt2,
                &[tagged_leak, "bad tags", late_leak, "[Source Text]"],
            );
            backend.hy_mt2_1_8b = small;
            let (results, stats) = run_units(&mut backend, &request);
            assert_eq!(results, [UnitResult::Invalid]);
            assert_eq!(stats.prompt_leaks, 3);
            assert_eq!(backend.stopped_early, 0);
            assert_eq!(
                backend.calls,
                [
                    (TAGGED_SOURCE.into(), false),
                    (TAGGED_SOURCE.into(), true),
                    (plain.into(), false),
                    (plain.into(), true),
                ]
            );
        }
        let mut plamo = Scripted::finished(PlamoPfml, &[TAGGED_TARGET]);
        assert_eq!(
            run_units(&mut plamo, &request).0,
            [UnitResult::Complete {
                text: TAGGED_TARGET.into(),
                retried: false,
            }]
        );
        assert_eq!(plamo.calls, [(TAGGED_SOURCE.into(), false)]);
    }

    #[test]
    fn plamo_does_not_retry_an_invalid_office_anchor() {
        use crate::profiles::ModelProfile::PlamoPfml;
        let mut backend = Scripted::finished(PlamoPfml, &["[Source Text]"]);
        let (results, stats) = run_units(&mut backend, &anchor_request(Some("A sample text.")));
        assert_eq!(results, [UnitResult::Invalid]);
        assert_eq!(stats.retries, 0);
        assert_eq!(stats.prompt_leaks, 0);
        assert_eq!(backend.calls, [(TAGGED_SOURCE.into(), false)]);
    }

    #[test]
    fn index_retries_office_anchors_then_uses_only_a_valid_plain_fallback() {
        use crate::profiles::ModelProfile::IndexTranslate;
        let plain = "A sample text.";
        for (fallback, replies, expected) in [
            (
                Some(plain),
                vec!["Broken", TAGGED_TARGET],
                UnitResult::Complete {
                    text: TAGGED_TARGET.into(),
                    retried: true,
                },
            ),
            (
                Some(plain),
                vec!["Broken", "Still broken", "Exemple de texte."],
                UnitResult::Simplified {
                    text: "Exemple de texte.".into(),
                },
            ),
            (None, vec!["Broken", "Still broken"], UnitResult::Invalid),
        ] {
            let used_fallback = replies.len() == 3;
            let mut backend = Scripted::finished(IndexTranslate, &replies);
            let (results, stats) = run_units(&mut backend, &anchor_request(fallback));
            assert_eq!(results, [expected]);
            assert_eq!(stats.retries, 1);
            let mut calls = vec![(TAGGED_SOURCE.into(), false), (TAGGED_SOURCE.into(), true)];
            if used_fallback {
                calls.push((plain.into(), false));
            }
            assert_eq!(backend.calls, calls);
        }
    }

    #[test]
    fn index_does_not_retry_an_invalid_markdown_anchor() {
        use crate::profiles::ModelProfile::IndexTranslate;
        let mut request = anchor_request(None);
        request.tuning = UnitTuning::default();
        let mut backend = Scripted::finished(IndexTranslate, &["[Source Text]"]);
        let (results, stats) = run_units(&mut backend, &request);
        assert_eq!(results, [UnitResult::Invalid]);
        assert_eq!(stats.retries, 0);
        assert_eq!(stats.prompt_leaks, 0);
        assert_eq!(backend.calls, [(TAGGED_SOURCE.into(), false)]);
    }

    #[test]
    fn hy_mt2_plain_units_keep_source_only_when_leaks_persist() {
        use crate::profiles::ModelProfile::HyMt2;
        let mut backend = Scripted::finished(
            HyMt2,
            &[
                "[Source Text]",
                "Traduction",
                "Output only the translation.",
                "[Source Text]",
                "Voir [Source Text].",
            ],
        );
        let request = UnitRequest {
            units: ["first", "second", "See [Source Text]."]
                .into_iter()
                .map(|text| Unit {
                    text: text.into(),
                    indivisible: false,
                })
                .collect(),
            tuning: UnitTuning::default(),
            max_unit_tokens: 100,
            max_history_tokens: 0,
            validate: Arc::new(|_, _| true),
        };
        let (results, stats) = run_units(&mut backend, &request);
        assert_eq!(
            results,
            [
                UnitResult::Complete {
                    text: "Traduction".into(),
                    retried: true
                },
                UnitResult::PromptLeak,
                UnitResult::Complete {
                    text: "Voir [Source Text].".into(),
                    retried: false
                },
            ]
        );
        assert_eq!(stats.prompt_leaks, 3);
    }

    #[test]
    fn pdf_units_stop_a_leading_header_and_retry_only_small_model_in_sentences() {
        use crate::profiles::ModelProfile::{HyMt2, PlamoPfml};

        let source = "First sentence. Second sentence.";
        let request = UnitRequest {
            units: vec![Unit {
                text: source.into(),
                indivisible: false,
            }],
            tuning: UnitTuning {
                recover_prompt_leaks: true,
                ..Default::default()
            },
            max_unit_tokens: 100,
            max_history_tokens: 0,
            validate: Arc::new(|_, _| true),
        };
        let translated = "Première phrase. Deuxième phrase.";
        let leaked = "[Source Text] extra words";
        for (profile, small, replies, calls) in [
            (
                HyMt2,
                true,
                vec![leaked, "Première phrase.", "Deuxième phrase."],
                vec![
                    (source, false),
                    ("First sentence.", false),
                    ("Second sentence.", false),
                ],
            ),
            (
                HyMt2,
                false,
                vec![leaked, translated],
                vec![(source, false), (source, true)],
            ),
            (PlamoPfml, false, vec![translated], vec![(source, false)]),
        ] {
            let mut backend = Scripted::finished(profile, &replies);
            backend.hy_mt2_1_8b = small;
            let (results, stats) = run_units(&mut backend, &request);
            assert_eq!(
                results,
                [UnitResult::Complete {
                    text: translated.into(),
                    retried: profile == HyMt2,
                }]
            );
            assert_eq!(
                backend.calls,
                calls
                    .into_iter()
                    .map(|(text, penalized)| (text.to_owned(), penalized))
                    .collect::<Vec<_>>()
            );
            assert_eq!(backend.stopped_early, usize::from(small));
            assert_eq!(stats.prompt_leaks, usize::from(profile == HyMt2));
        }
    }

    #[test]
    fn pdf_keeps_the_entire_paragraph_when_a_retried_sentence_still_leaks() {
        use crate::profiles::ModelProfile::HyMt2;

        let leak = "[Source Text] extra words";
        let mut backend = Scripted::finished(HyMt2, &[leak, "Premier.", leak, leak, "Dernier."]);
        backend.hy_mt2_1_8b = true;
        let request = UnitRequest {
            units: ["First. Second.", "Last."]
                .into_iter()
                .map(|text| Unit {
                    text: text.into(),
                    indivisible: false,
                })
                .collect(),
            tuning: UnitTuning {
                recover_prompt_leaks: true,
                ..Default::default()
            },
            max_unit_tokens: 100,
            max_history_tokens: 0,
            validate: Arc::new(|_, _| true),
        };
        let (results, stats) = run_units(&mut backend, &request);
        assert_eq!(
            results,
            [
                UnitResult::Cut,
                UnitResult::Complete {
                    text: "Dernier.".into(),
                    retried: false,
                },
            ]
        );
        assert_eq!(stats.cut, 1);
        assert_eq!(stats.prompt_leaks, 3);
        assert_eq!(backend.stopped_early, 3);
    }

    #[test]
    fn hy_mt2_plain_text_rewinds_both_leaks_and_reports_a_cut_when_retry_fails() {
        use crate::document::{DocumentOptions, Output, Progress, translate_document};
        use crate::profiles::ModelProfile::HyMt2;

        let mut backend = Scripted::finished(
            HyMt2,
            &[
                "[Source Text]",
                "Output only the translation.",
                "Traduction",
            ],
        );
        let source = "First.\n\nSecond.";
        let mut options = TranslationOptions::new(LangCode::new("fr").unwrap());
        options.source = Some(LangCode::new("en").unwrap());
        let mut text = String::new();
        let mut rewinds = Vec::new();
        let mut cuts = Vec::new();
        let stats = translate_document(
            &mut backend,
            &str::len,
            4096,
            &DocumentOptions {
                max_chunk_tokens: 8,
                cut_marker: "_".into(),
                ..Default::default()
            },
            &options,
            source,
            &mut |output| match output {
                Output::Text(piece) => text.push_str(piece),
                Output::Truncate(len) => {
                    rewinds.push(len);
                    text.truncate(len);
                }
            },
            &mut |progress| {
                if let Progress::Cut { source, output, .. } = progress {
                    cuts.push((source, output));
                }
            },
            &|| false,
        )
        .unwrap();
        assert_eq!(text, "_\n\nTraduction");
        assert_eq!(rewinds, [0, 0]);
        assert_eq!(cuts, [((0, 6), (0, 1))]);
        assert_eq!(stats.prompt_leaks, 2);
        assert_eq!(stats.cut, 1);
    }
}
