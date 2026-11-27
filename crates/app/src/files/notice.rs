//! File notices keep their translation keys and counts until the panel is rendered.
use ::files::{
    FileError, office,
    pdf::{
        Error as Pdf,
        export::{PageKind, SavedDocument},
    },
};
use engine::profiles::FileFormat;
use rust_i18n::t;

/// The formats a model translates, as the file tab names them.
pub(super) fn format_names(formats: &[FileFormat]) -> String {
    formats
        .iter()
        .map(|&format| super::format_label(format))
        .collect::<Vec<_>>()
        .join(" · ")
}

#[derive(Clone, Copy)]
pub(super) enum Operation {
    Open,
    Save,
}

pub(super) enum FileNotice {
    Message {
        key: &'static str,
        detail: Option<String>,
    },
    Layout(LayoutSummary),
    /// The loaded model cannot translate this format; these are the ones it can.
    ModelFormats {
        formats: &'static [FileFormat],
        format: FileFormat,
    },
}

impl FileNotice {
    pub(super) fn text(key: &'static str) -> Self {
        Self::Message { key, detail: None }
    }

    pub(super) fn failure(key: &'static str, detail: String) -> Self {
        Self::Message {
            key,
            detail: Some(detail),
        }
    }

    pub(super) fn error(error: &FileError, operation: Operation) -> Self {
        let key = match error {
            FileError::Folder => "files.folder",
            FileError::Unsupported => "files.unsupported",
            FileError::TooLarge => "files.too_large",
            FileError::Encoding => "files.bad_encoding",
            FileError::OriginalDestination => "files.original_destination",
            FileError::PathTooLong => "files.path_too_long",
            FileError::DestinationExists => "files.destination_exists",
            FileError::NoTranslation => "files.no_changes",
            FileError::SourceFolder(error)
                if error.kind() == std::io::ErrorKind::PermissionDenied =>
            {
                "files.source_folder_permission"
            }
            FileError::SourceFolder(error) => {
                return Self::failure("files.save_beside_failed", error.to_string());
            }
            FileError::Pdf(Pdf::Encrypted) => "files.pdf_password",
            FileError::Pdf(Pdf::EditingRestricted) => "files.pdf_edit_restricted",
            FileError::Pdf(Pdf::Protected) => "files.pdf_protected",
            FileError::Pdf(Pdf::Library) => "files.pdf_library",
            FileError::Pdf(Pdf::UnsupportedLanguage) => "files.pdf_text_output",
            FileError::Pdf(Pdf::Font) => "files.pdf_font",
            FileError::Pdf(Pdf::Limit) => "files.pdf_limit",
            FileError::Pdf(Pdf::Interactive) => "files.pdf_interactive",
            FileError::Office(office::Error::Limit) => "files.office_limit",
            FileError::Office(
                office::Error::Package | office::Error::Xml | office::Error::Zip(_),
            ) if matches!(operation, Operation::Open) => "files.office_package",
            _ => {
                return Self::failure(
                    match operation {
                        Operation::Open => "files.open_failed",
                        Operation::Save => "files.save_failed",
                    },
                    error.to_string(),
                );
            }
        };
        Self::text(key)
    }

    pub(super) fn batch_preflight_error(error: &FileError) -> Self {
        // The preflight runs before there is a translation to save elsewhere.
        match error {
            FileError::SourceFolder(e) if e.kind() == std::io::ErrorKind::PermissionDenied => {
                Self::text("files.batch_folder_permission")
            }
            FileError::SourceFolder(e) => Self::failure("files.batch_folder_failed", e.to_string()),
            FileError::PathTooLong => Self::text("files.batch_path_long"),
            _ => Self::error(error, Operation::Save),
        }
    }

    pub(super) fn model_formats(formats: &'static [FileFormat], format: FileFormat) -> Self {
        Self::ModelFormats { formats, format }
    }

    pub(super) fn summary(&self) -> String {
        match self {
            Self::Message { key, .. } => t!(*key).into_owned(),
            Self::Layout(layout) => layout.summary(),
            Self::ModelFormats { formats, format } => {
                let formats = t!("files.model_formats", formats = format_names(formats));
                let guidance = match format {
                    FileFormat::Word => t!("files.switch_for_word"),
                    FileFormat::PowerPoint => t!("files.switch_for_powerpoint"),
                    FileFormat::Pdf => t!("files.switch_for_pdf"),
                    _ => return formats.into_owned(),
                };
                format!("{formats} {guidance}")
            }
        }
    }

    pub(super) fn details(&self) -> Option<String> {
        match self {
            Self::Message { detail, .. } => detail.clone(),
            Self::Layout(layout) => Some(layout.details()),
            Self::ModelFormats { .. } => None,
        }
    }

    pub(super) fn success(&self) -> bool {
        matches!(self, Self::Layout(s) if s.bilingual == 0 && s.untranslated == 0 && !s.markers && !s.fallback)
    }
}

#[derive(Default)]
pub(super) struct LayoutSummary {
    originals: usize,
    bilingual: usize,
    added: usize,
    supplement: usize,
    layout_without_placement: usize,
    paragraphs: usize,
    untranslated: usize,
    repeated: usize,
    markers: bool,
    fallback: bool,
}

impl LayoutSummary {
    pub(super) fn from_export(export: &SavedDocument) -> Self {
        let mut summary = Self {
            fallback: export.fallback_error.is_some(),
            ..Self::default()
        };
        for page in &export.pages {
            summary.count_page(page.kind, page.added_pages, !page.placed.is_empty());
            summary.paragraphs += page.placed.len() + page.appended.len() + page.repeated.len();
            summary.untranslated += page.untranslated.len();
            summary.repeated += page.repeated.len();
            summary.markers |= page.has_square_markers;
        }
        summary
    }

    fn count_page(&mut self, kind: PageKind, added: usize, has_placement: bool) {
        if kind != PageKind::Bilingual && !has_placement {
            self.layout_without_placement += 1;
        }
        self.originals += 1;
        self.added += added;
        match kind {
            PageKind::Layout => {}
            PageKind::Supplement => self.supplement += added,
            PageKind::Bilingual => self.bilingual += 1,
        }
    }

    pub(super) fn warning(&self) -> Option<String> {
        if self.bilingual > 0 {
            Some(
                t!(
                    if self.bilingual != self.originals {
                        "files.batch_bilingual"
                    } else if self.originals == 1 {
                        "files.batch_bilingual_only_page"
                    } else {
                        "files.batch_bilingual_all"
                    },
                    total = self.originals,
                    count = self.bilingual
                )
                .into_owned(),
            )
        } else if self.untranslated > 0 {
            Some(
                t!(
                    if self.untranslated == 1 {
                        "files.pdf_untranslated_one"
                    } else {
                        "files.pdf_untranslated"
                    },
                    count = self.untranslated
                )
                .into_owned(),
            )
        } else if self.fallback {
            Some(t!("files.pdf_layout_fallback").into_owned())
        } else if self.markers {
            Some(t!("files.pdf_square_markers").into_owned())
        } else {
            None
        }
    }

    fn summary(&self) -> String {
        let mut lines = Vec::new();
        if self.bilingual == self.originals {
            lines.push(t!("files.pdf_all_bilingual", total = self.originals).into_owned());
        } else {
            lines.push(
                t!(
                    "files.pdf_layout_saved",
                    layout = self.originals - self.bilingual,
                    total = self.originals
                )
                .into_owned(),
            );
            if self.bilingual > 0 {
                lines.push(
                    t!(
                        if self.bilingual == 1 {
                            "files.pdf_bilingual_count_one"
                        } else {
                            "files.pdf_bilingual_count"
                        },
                        count = self.bilingual
                    )
                    .into_owned(),
                );
            }
        }
        if self.untranslated > 0 {
            lines.push(
                t!(
                    if self.untranslated == 1 {
                        "files.pdf_untranslated_one"
                    } else {
                        "files.pdf_untranslated"
                    },
                    count = self.untranslated
                )
                .into_owned(),
            );
        }
        if self.bilingual == 0 && self.untranslated == 0 {
            if self.fallback {
                lines.push(t!("files.pdf_layout_fallback").into_owned());
            } else if self.markers {
                lines.push(t!("files.pdf_square_markers").into_owned());
            }
        }
        lines
            .into_iter()
            .reduce(|first, next| t!("files.pdf_join", first = first, next = next).into_owned())
            .unwrap_or_default()
    }

    fn details(&self) -> String {
        let mut lines = vec![
            t!(
                "files.pdf_output_pages",
                added = self.added,
                total = self.originals + self.added
            )
            .into_owned(),
        ];
        if self.layout_without_placement > 0 {
            lines.push(
                t!(
                    if self.layout_without_placement == 1 {
                        "files.pdf_no_placement_one"
                    } else {
                        "files.pdf_no_placement"
                    },
                    count = self.layout_without_placement
                )
                .into_owned(),
            );
        }
        if self.supplement > 0 {
            lines.push(
                t!(
                    if self.supplement == 1 {
                        "files.pdf_supplement_saved_one"
                    } else {
                        "files.pdf_supplement_saved"
                    },
                    count = self.supplement
                )
                .into_owned(),
            );
        }
        lines.push(
            t!(
                if self.paragraphs == 1 {
                    "files.pdf_paragraphs_saved_one"
                } else {
                    "files.pdf_paragraphs_saved"
                },
                count = self.paragraphs
            )
            .into_owned(),
        );
        if self.repeated > 0 {
            lines.push(
                t!(
                    if self.repeated == 1 {
                        "files.pdf_running_consolidated_one"
                    } else {
                        "files.pdf_running_consolidated"
                    },
                    count = self.repeated
                )
                .into_owned(),
            );
        }
        if self.markers {
            lines.push(t!("files.pdf_square_markers").into_owned());
        }
        if self.fallback {
            lines.push(t!("files.pdf_layout_fallback").into_owned());
        }
        lines.join("\n")
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn known_errors_are_localized_and_unexpected_details_are_separate() {
        for (error, expected) in [
            (FileError::Folder, "files.folder"),
            (FileError::Unsupported, "files.unsupported"),
            (FileError::TooLarge, "files.too_large"),
            (FileError::Pdf(Pdf::Limit), "files.pdf_limit"),
            (FileError::Encoding, "files.bad_encoding"),
            (
                FileError::Office(office::Error::Limit),
                "files.office_limit",
            ),
            (FileError::OriginalDestination, "files.original_destination"),
            (FileError::DestinationExists, "files.destination_exists"),
            (FileError::PathTooLong, "files.path_too_long"),
        ] {
            for op in [Operation::Open, Operation::Save] {
                assert!(
                    matches!(FileNotice::error(&error, op), FileNotice::Message { key, detail: None } if key == expected)
                );
            }
        }
        let e = FileError::PdfPanic("diagnostic".into());
        for (op, expected) in [
            (Operation::Open, "files.open_failed"),
            (Operation::Save, "files.save_failed"),
        ] {
            let notice = FileNotice::error(&e, op);
            assert!(matches!(notice, FileNotice::Message {key,..} if key == expected));
            assert_eq!(notice.details().as_deref(), Some(e.to_string().as_str()));
            assert!(!notice.summary().contains("diagnostic"));
        }
    }

    #[test]
    fn warning_with_only_markers_or_fallback_explains_itself_in_the_summary() {
        let _lock = crate::lang::TEST_LOCALE
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        let previous = rust_i18n::locale().to_string();
        for locale in ["en", "ja"] {
            rust_i18n::set_locale(locale);
            for (markers, fallback, key) in [
                (true, false, "files.pdf_square_markers"),
                (false, true, "files.pdf_layout_fallback"),
            ] {
                let notice = FileNotice::Layout(LayoutSummary {
                    originals: 1,
                    markers,
                    fallback,
                    ..Default::default()
                });
                assert!(!notice.success());
                assert!(notice.summary().contains(t!(key).as_ref()));
            }
        }
        rust_i18n::set_locale(&previous);
    }

    #[test]
    fn bilingual_row_warning_distinguishes_one_all_and_some_pages() {
        let _lock = crate::lang::TEST_LOCALE
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        let previous = rust_i18n::locale().to_string();
        for (count, total, ja, en) in [
            (
                1,
                1,
                "全1ページを原文と訳文を並べて保存しました。",
                "Saved the page with the original and translation side by side.",
            ),
            (
                3,
                3,
                "全3ページを原文と訳文を並べて保存しました。",
                "Saved all 3 pages with the original and translation side by side.",
            ),
            (
                2,
                3,
                "3ページ中2ページは原文と訳文を並べました。",
                "Bilingual format was used on 2 of 3 pages.",
            ),
        ] {
            let summary = LayoutSummary {
                originals: total,
                bilingual: count,
                ..Default::default()
            };
            for (locale, expected) in [("ja", ja), ("en", en)] {
                rust_i18n::set_locale(locale);
                assert_eq!(summary.warning().as_deref(), Some(expected));
            }
        }
        rust_i18n::set_locale(&previous);
    }

    #[test]
    fn layout_summary_and_details_use_exported_page_kinds_and_keep_warnings() {
        let _lock = crate::lang::TEST_LOCALE
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        let previous = rust_i18n::locale().to_string();
        let mut s = LayoutSummary {
            paragraphs: 14,
            repeated: 2,
            untranslated: 1,
            markers: true,
            fallback: true,
            ..Default::default()
        };
        s.count_page(PageKind::Bilingual, 2, false);
        for locale in ["ja", "en"] {
            rust_i18n::set_locale(locale);
            assert_eq!(
                s.summary(),
                if locale == "ja" {
                    "全1ページで原文と訳文を並べました。1段落は訳されませんでした。"
                } else {
                    "Saved every source page in bilingual format (1 total). 1 paragraph remained untranslated."
                }
            );
        }
        s.count_page(PageKind::Layout, 0, false);
        s.count_page(PageKind::Supplement, 3, true);
        rust_i18n::set_locale("ja");
        assert_eq!(
            s.summary(),
            "3ページ中2ページで元のレイアウトを保ちました。1ページは原文と訳文を並べました。1段落は訳されませんでした。"
        );
        let details = s.details();
        for expected in [
            "訳文を入れずに保存したページが1ページ",
            "追加5ページ（全8ページ）",
            "補足ページを3ページ",
            "14段落",
            "見出し2段落",
            "□",
            "安全に置き換えられない",
        ] {
            assert!(details.contains(expected), "{details}");
        }
        rust_i18n::set_locale(&previous);
    }
}
