//! Product capabilities shared by the desktop and inference profiles.

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Hash)]
pub enum ModelProfile {
    #[default]
    PlamoPfml,
    HyMt2,
    IndexTranslate,
}

#[derive(Clone, Copy, PartialEq, Eq)]
pub enum FileFormat {
    Txt,
    Markdown,
    Word,
    PowerPoint,
    Pdf,
}

impl FileFormat {
    pub const ALL: &'static [Self] = &[
        Self::Pdf,
        Self::Word,
        Self::PowerPoint,
        Self::Markdown,
        Self::Txt,
    ];

    pub const fn is_document(self) -> bool {
        matches!(self, Self::Word | Self::PowerPoint | Self::Pdf)
    }
}

#[derive(Clone, Copy, PartialEq, Eq)]
pub enum ModelLicense {
    PlamoCommunity,
    Apache2,
}

pub struct Capabilities {
    /// Allowed explicit source and target pairs. PLaMo uses its original language and auto rules.
    pub directions: Option<&'static [(&'static str, &'static str)]>,
    pub files: &'static [FileFormat],
    pub japanese_style: bool,
    pub license: ModelLicense,
    pub context_limit: Option<u32>,
}

pub(crate) struct ExternalLanguage {
    pub english: &'static str,
    pub chinese: &'static str,
}

fn external_language(code: &str) -> Option<ExternalLanguage> {
    let (english, chinese) = match code {
        "en" => ("English", "英语"),
        "ja" => ("Japanese", "日语"),
        "fr" => ("French", "法语"),
        "de" => ("German", "德语"),
        "pt" => ("Brazilian Portuguese", "巴西葡萄牙语"),
        "id" => ("Indonesian", "印度尼西亚语"),
        "ar" => ("Arabic", "阿拉伯语"),
        "zh-hans" => ("Simplified Chinese", "简体中文"),
        "es" => ("Spanish", "西班牙语"),
        "ko" => ("Korean", "韩语"),
        _ => return None,
    };
    Some(ExternalLanguage { english, chinese })
}

/// The code of a language an external profile translates, also one PLaMo does not list (such
/// as `pt`), as a static string: to recognise earlier translations by their file names.
pub fn external_language_code(code: &str) -> Option<&'static str> {
    HY_MT2_DIRECTIONS
        .iter()
        .chain(INDEX_DIRECTIONS)
        .flat_map(|&(from, to)| [from, to])
        .find(|&known| known == code)
}

impl Capabilities {
    pub fn targets(&self, source: Option<&str>) -> impl Iterator<Item = &'static str> {
        self.directions
            .into_iter()
            .flatten()
            .filter(move |&&(from, _)| source == Some(from))
            .map(|&(_, to)| to)
    }
}

// "pt" is the internal language code; both external prompts specify Brazilian Portuguese.
const INDEX_DIRECTIONS: &[(&str, &str)] = &[
    ("en", "ja"),
    ("en", "fr"),
    ("en", "de"),
    ("en", "pt"),
    ("en", "id"),
    ("en", "ar"),
    ("ja", "en"),
    ("fr", "en"),
    ("de", "en"),
    ("pt", "en"),
    ("id", "en"),
    ("ar", "en"),
];

const HY_MT2_DIRECTIONS: &[(&str, &str)] = &[
    ("en", "ja"),
    ("en", "fr"),
    ("en", "de"),
    ("en", "pt"),
    ("en", "id"),
    ("en", "ar"),
    ("en", "zh-hans"),
    ("en", "es"),
    ("en", "ko"),
    ("ja", "en"),
    ("fr", "en"),
    ("de", "en"),
    ("pt", "en"),
    ("id", "en"),
    ("ar", "en"),
    ("zh-hans", "en"),
    ("es", "en"),
    ("ko", "en"),
];

pub(crate) const HY_MT2_CAPABILITIES: Capabilities = Capabilities {
    directions: Some(HY_MT2_DIRECTIONS),
    files: &[FileFormat::Markdown, FileFormat::Txt],
    japanese_style: false,
    license: ModelLicense::Apache2,
    context_limit: Some(8192),
};

impl ModelProfile {
    pub fn capabilities(self) -> &'static Capabilities {
        match self {
            Self::PlamoPfml => &Capabilities {
                directions: None,
                // In the order the file tab has always named them.
                files: FileFormat::ALL,
                japanese_style: true,
                license: ModelLicense::PlamoCommunity,
                context_limit: None,
            },
            Self::HyMt2 => &HY_MT2_CAPABILITIES,
            Self::IndexTranslate => &Capabilities {
                directions: Some(INDEX_DIRECTIONS),
                files: &[FileFormat::Markdown, FileFormat::Txt, FileFormat::Word],
                japanese_style: false,
                license: ModelLicense::Apache2,
                context_limit: Some(8192),
            },
        }
    }

    pub(crate) fn explicit_direction(
        self,
        options: &pfml::TranslationOptions,
    ) -> crate::Result<(ExternalLanguage, ExternalLanguage)> {
        let source = options.source.as_ref().map(pfml::LangCode::as_str);
        let target = options.target.as_str();
        if let Some(source) = source
            && self
                .capabilities()
                .targets(Some(source))
                .any(|to| to == target)
            && let (Some(source), Some(target)) =
                (external_language(source), external_language(target))
        {
            Ok((source, target))
        } else {
            Err(crate::EngineError::UnsupportedDirection(self))
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn external_language_codes_include_languages_plamo_does_not_list() {
        assert_eq!(external_language_code("pt"), Some("pt"));
        assert_eq!(external_language_code("en"), Some("en"));
        assert_eq!(external_language_code("ko"), Some("ko"));
        assert_eq!(external_language_code("zh-hant"), None);
        assert!(pfml::find_language("pt").is_none());
    }

    #[test]
    fn external_profiles_expose_only_verified_english_pairs() {
        for profile in [ModelProfile::HyMt2, ModelProfile::IndexTranslate] {
            let caps = profile.capabilities();
            let targets = caps.targets(Some("en")).collect::<Vec<_>>();
            let mut expected = vec!["ja", "fr", "de", "pt", "id", "ar"];
            if profile == ModelProfile::HyMt2 {
                expected.extend(["zh-hans", "es", "ko"]);
            }
            assert_eq!(targets, expected);
            assert_eq!(caps.directions.unwrap().len(), 2 * targets.len());
            for source in targets {
                assert_eq!(caps.targets(Some(source)).collect::<Vec<_>>(), ["en"]);
            }
            for &(source, target) in caps.directions.unwrap() {
                let mut options =
                    pfml::TranslationOptions::new(pfml::LangCode::new(target).unwrap());
                options.source = Some(pfml::LangCode::new(source).unwrap());
                assert!(
                    profile.explicit_direction(&options).is_ok(),
                    "{source} → {target}"
                );
            }
            assert!(caps.targets(None).next().is_none());
            let files: &[_] = if profile == ModelProfile::HyMt2 {
                &[FileFormat::Markdown, FileFormat::Txt]
            } else {
                &[FileFormat::Markdown, FileFormat::Txt, FileFormat::Word]
            };
            assert!(caps.files == files);
            assert!(!caps.japanese_style);
        }
    }
}
