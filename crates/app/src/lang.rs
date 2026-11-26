//! Choices offered in selects (languages, style, UI language, theme, token counts) and
//! localized formatting helpers.

#[cfg(test)]
pub(crate) static TEST_LOCALE: std::sync::Mutex<()> = std::sync::Mutex::new(());

use std::{borrow::Cow, time::Duration};

use engine::llama::ModelProfile;
use engine::pfml::{JA_EASY, LANGUAGES, find_language};
use gpui_kit::component::select::SelectItem;
use gpui_kit::{AnyElement, AnyView, IntoElement, SharedString};
use rust_i18n::t;

use crate::settings::{LanguagePair, Settings, ThemeSetting, UiLanguage};

/// Source language choice.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SourceChoice {
    /// Let the model detect the language.
    Detect,
    Lang(&'static str),
}

impl SourceChoice {
    pub fn code(self) -> Option<&'static str> {
        match self {
            Self::Detect => None,
            Self::Lang(code) => Some(code),
        }
    }
}

pub use appdata::settings::{StyleChoice, TargetChoice};

pub use appdata::looks_japanese;

/// Localized display name of a language code.
pub fn language_name(code: &str) -> String {
    let key = format!("lang.{}", code.replace('-', "_").replace("(easy)", "_easy"));
    crate::_rust_i18n_try_translate(&rust_i18n::locale(), &key)
        .map(Cow::into_owned)
        .unwrap_or_else(|| {
            find_language(code).map_or_else(|| code.to_owned(), |l| l.english_name.to_owned())
        })
}

/// An entry in a select.
#[derive(Debug, Clone)]
pub struct Choice<V> {
    value: V,
    title: SharedString,
    trigger_title: Option<AnyView>,
    /// Lowercased text matched against the search query.
    keywords: String,
}

impl<V> Choice<V> {
    pub fn new(value: V, title: String, extra_keywords: &[&str]) -> Self {
        let mut keywords = title.to_lowercase();
        for k in extra_keywords {
            keywords.push('\n');
            keywords.push_str(&k.to_lowercase());
        }
        Self {
            value,
            title: title.into(),
            trigger_title: None,
            keywords,
        }
    }

    pub(crate) fn set_trigger_title(&mut self, title: AnyView) {
        self.trigger_title = Some(title);
    }

    pub fn language(value: V, code: &'static str) -> Self {
        let mut extra = vec![code];
        if let Some(l) = find_language(code) {
            extra.extend([l.english_name, l.native_name]);
        }
        Self::new(value, language_name(code), &extra)
    }
}

impl<V: Clone + PartialEq> SelectItem for Choice<V> {
    type Value = V;

    fn title(&self) -> SharedString {
        self.title.clone()
    }

    fn display_title(&self) -> Option<AnyElement> {
        self.trigger_title
            .clone()
            .map(IntoElement::into_any_element)
    }

    fn value(&self) -> &V {
        &self.value
    }

    fn matches(&self, query: &str) -> bool {
        self.keywords.contains(&query.to_lowercase())
    }
}

pub fn source_choices() -> Vec<Choice<SourceChoice>> {
    let mut choices = vec![Choice::new(
        SourceChoice::Detect,
        t!("source.detect").into_owned(),
        &["auto"],
    )];
    choices.extend(
        LANGUAGES
            .iter()
            .map(|l| Choice::language(SourceChoice::Lang(l.code), l.code)),
    );
    choices
}

pub fn target_choices() -> Vec<Choice<TargetChoice>> {
    let mut choices = vec![Choice::new(
        TargetChoice::JapaneseEnglish,
        t!("target.ja_en").into_owned(),
        &["auto", "ja", "en"],
    )];
    choices.extend(
        LANGUAGES
            .iter()
            .map(|l| Choice::language(TargetChoice::Lang(l.code), l.code)),
    );
    choices.push(Choice::language(TargetChoice::Lang(JA_EASY), JA_EASY));
    choices
}

pub fn profile_source_choices(profile: ModelProfile) -> Vec<Choice<SourceChoice>> {
    let Some(directions) = profile.capabilities().directions else {
        return source_choices();
    };
    let mut sources = Vec::new();
    for &(from, _) in directions {
        if !sources.contains(&from) {
            sources.push(from);
        }
    }
    sources
        .into_iter()
        .map(|code| Choice::language(SourceChoice::Lang(code), code))
        .collect()
}

pub fn profile_targets(
    profile: ModelProfile,
    source: SourceChoice,
) -> impl Iterator<Item = &'static str> {
    profile.capabilities().targets(source.code())
}

/// The languages offered and chosen for the loaded model.
///
/// PLaMo offers every choice. Its source is not saved, and its target is the saved
/// `target_language`. An external model translates fixed pairs, and the pair last chosen with
/// one is saved apart, in `external_languages`. Neither kind of model writes the other's
/// choice, so each gets back what was last chosen with it, also after a restart or a fallback.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Languages {
    pub profile: ModelProfile,
    pub automatic: bool,
    /// The source for the loaded model.
    pub source: SourceChoice,
    /// PLaMo's source while an external model is loaded.
    plamo_source: SourceChoice,
    /// The target while an external model is loaded.
    external_target: &'static str,
}

impl Default for Languages {
    fn default() -> Self {
        Self {
            profile: ModelProfile::default(),
            automatic: false,
            source: SourceChoice::Detect,
            plamo_source: SourceChoice::Detect,
            external_target: "ja",
        }
    }
}

impl Languages {
    /// Whether the loaded model translates fixed pairs only.
    pub fn explicit(&self) -> bool {
        !self.automatic && self.profile.capabilities().directions.is_some()
    }

    pub fn target(&self, settings: &Settings) -> TargetChoice {
        if self.explicit() {
            TargetChoice::Lang(self.external_target)
        } else {
            settings.target_language
        }
    }

    /// Offers `profile`'s languages: for PLaMo its own source and saved target; for an external
    /// model the saved pair if it translates that pair, else its first one. Saves nothing.
    pub fn load(&mut self, profile: ModelProfile, settings: &Settings) {
        if self.automatic {
            self.profile = profile;
            return;
        }
        if !self.explicit() {
            self.plamo_source = self.source;
        }
        self.profile = profile;
        let Some(directions) = profile.capabilities().directions else {
            self.source = if self
                .plamo_source
                .code()
                .is_none_or(|code| engine::pfml::find_language(code).is_some())
            {
                self.plamo_source
            } else {
                SourceChoice::Detect
            };
            return;
        };
        let saved = settings.external_languages.as_ref().and_then(|saved| {
            directions
                .iter()
                .find(|&&(from, to)| saved.source == from && saved.target == to)
        });
        if let Some(&(source, target)) = saved.or(directions.first()) {
            self.source = SourceChoice::Lang(source);
            self.external_target = target;
        }
    }

    /// Keep as much of the current pair as the pinned model supports.
    pub fn pin(&mut self, profile: ModelProfile, settings: &mut Settings) -> bool {
        let source = self.source;
        let target = self.target(settings);
        self.automatic = false;
        self.load(profile, settings);

        if let Some(directions) = profile.capabilities().directions {
            let concrete_target = target.resolve_for(source.code() == Some("ja"));
            if let Some(&(from, to)) = directions
                .iter()
                .min_by_key(|&&(from, to)| (Some(from) != source.code(), to != concrete_target))
            {
                self.source = SourceChoice::Lang(from);
                self.external_target = to;
                self.save_pair(settings);
            }
        } else {
            self.source = source
                .code()
                .filter(|code| engine::pfml::find_language(code).is_some())
                .map_or(SourceChoice::Detect, SourceChoice::Lang);
            settings.target_language = match target {
                TargetChoice::Lang("pt") => TargetChoice::Lang(if source.code() == Some("en") {
                    "ja"
                } else {
                    "en"
                }),
                _ => target,
            };
        }
        self.source != source || self.target(settings) != target
    }

    /// The user chose `source`. Returns whether `settings` changed and are to be saved.
    pub fn choose_source(&mut self, source: SourceChoice, settings: &mut Settings) -> bool {
        if !self.explicit() {
            self.source = source;
            return false;
        }
        let Some(first) = profile_targets(self.profile, source).next() else {
            return false;
        };
        if !profile_targets(self.profile, source).any(|code| code == self.external_target) {
            self.external_target = first;
        }
        self.source = source;
        self.save_pair(settings)
    }

    /// The user chose `target`. Returns whether `settings` changed and are to be saved.
    pub fn choose_target(&mut self, target: TargetChoice, settings: &mut Settings) -> bool {
        if !self.explicit() {
            let changed = settings.target_language != target;
            settings.target_language = target;
            return changed;
        }
        match target {
            TargetChoice::Lang(code)
                if profile_targets(self.profile, self.source).any(|to| to == code) =>
            {
                self.external_target = code;
                self.save_pair(settings)
            }
            _ => false,
        }
    }

    /// The swapped languages, if they can be swapped: with Japanese ⇄ English as PLaMo's target,
    /// an explicit source becomes the translation's language (the target stays).
    fn swapped(&self, settings: &Settings) -> Option<(SourceChoice, TargetChoice)> {
        // Easy Japanese is a target only: its text is Japanese as a source.
        let as_source = |code: &'static str| if code == JA_EASY { "ja" } else { code };
        let swapped = match (self.source, self.target(settings)) {
            (SourceChoice::Lang(s), TargetChoice::Lang(t)) => {
                (SourceChoice::Lang(as_source(t)), TargetChoice::Lang(s))
            }
            (SourceChoice::Lang(s), TargetChoice::JapaneseEnglish) => (
                SourceChoice::Lang(TargetChoice::JapaneseEnglish.resolve_for(s == "ja")),
                TargetChoice::JapaneseEnglish,
            ),
            _ => return None,
        };
        let reachable = |(source, target): (SourceChoice, TargetChoice)| {
            !self.explicit()
                || profile_targets(self.profile, source).any(|to| target == TargetChoice::Lang(to))
        };
        Some(swapped).filter(|&pair| reachable(pair))
    }

    /// Whether the swap button applies. With PLaMo's Japanese ⇄ English it also does after
    /// detection: the translation moves into the source and the languages stay.
    pub fn can_swap(&self, settings: &Settings) -> bool {
        if self.explicit() {
            self.swapped(settings).is_some()
        } else {
            matches!(
                (self.source, settings.target_language),
                (SourceChoice::Lang(_), TargetChoice::Lang(_)) | (_, TargetChoice::JapaneseEnglish)
            )
        }
    }

    /// Swaps the source and target. Returns whether they changed (and `settings` are to be
    /// saved).
    pub fn swap(&mut self, settings: &mut Settings) -> bool {
        let Some((source, target)) = self.swapped(settings) else {
            return false;
        };
        self.source = source;
        if let (true, TargetChoice::Lang(code)) = (self.explicit(), target) {
            self.external_target = code;
            self.save_pair(settings);
        } else {
            settings.target_language = target;
        }
        true
    }

    fn save_pair(&self, settings: &mut Settings) -> bool {
        let pair = self.source.code().map(|source| LanguagePair {
            source: source.to_owned(),
            target: self.external_target.to_owned(),
        });
        let changed = settings.external_languages != pair;
        settings.external_languages = pair;
        changed
    }

    pub fn source_choices(&self) -> Vec<Choice<SourceChoice>> {
        if self.automatic {
            let mut choices = source_choices();
            choices.push(Choice::language(SourceChoice::Lang("pt"), "pt"));
            choices
        } else {
            profile_source_choices(self.profile)
        }
    }

    pub fn target_choices(&self) -> Vec<Choice<TargetChoice>> {
        if self.explicit() {
            profile_targets(self.profile, self.source)
                .map(|code| Choice::language(TargetChoice::Lang(code), code))
                .collect()
        } else {
            let mut choices = target_choices();
            if self.automatic {
                choices.push(Choice::language(TargetChoice::Lang("pt"), "pt"));
            }
            choices
        }
    }
}

#[cfg(test)]
fn sample_source(code: Option<&str>) -> Option<&'static str> {
    Some(match code {
        Some("ja") => {
            "すべての人間は、生れながらにして自由であり、かつ、尊厳と権利とについて平等である。"
        }
        Some("fr") => "La bibliothèque est fermée le dimanche.",
        Some("de") => "Die Bibliothek ist sonntags geschlossen.",
        Some("pt") => "A biblioteca fecha aos domingos.",
        Some("id") => "Perpustakaan tutup pada hari Minggu.",
        Some("ar") => "المكتبة مغلقة أيام الأحد.",
        Some("zh-hans") => "请问这个车站怎么走？",
        Some("es") => {
            "La biblioteca permanece cerrada los domingos y durante las vacaciones de verano."
        }
        Some("ko") => "서류는 다음 주 월요일까지 제출해 주세요.",
        Some("en") => {
            "The committee met on Monday and agreed that the report would be published at the end of the month. The members also agreed to meet again in the autumn."
        }
        _ => return None,
    })
}

/// UI language options. Languages are named in their own language.
pub fn ui_language_choices() -> Vec<Choice<UiLanguage>> {
    let locale = crate::settings::system_locale();
    let options = UiLanguage::OPTIONS;
    let system = options
        .iter()
        .find_map(|&(_, tag, name)| (tag == locale).then_some(name))
        .unwrap_or("English");
    std::iter::once(Choice::new(
        UiLanguage::System,
        t!("settings.language_system", lang = system).into_owned(),
        &[],
    ))
    .chain(
        options
            .iter()
            .map(|&(language, tag, name)| Choice::new(language, name.to_owned(), &[tag])),
    )
    .collect()
}

/// Chunk size options, in tokens. Chunks are also capped at a sixth of the context, so larger
/// values would not take effect.
pub const CHUNK_TOKEN_PRESETS: &[usize] = &[256, 512, 1024, 2048, 4096];

/// History size options, in tokens. History is also capped at a third of the context.
pub const HISTORY_TOKEN_PRESETS: &[usize] = &[0, 1024, 2048, 4096, 8192];

/// The interface sizes offered, in percent.
pub const UI_SCALE_PRESETS: &[u16] = &[100, 125, 150, 175, 200];

/// Interface size options: the presets, plus `current` if it is not one of them (a value edited
/// in settings.json). 100% is marked as the default.
pub fn ui_scale_choices(current: u16) -> Vec<Choice<u16>> {
    let mut values = UI_SCALE_PRESETS.to_vec();
    if !values.contains(&current) {
        values.push(current);
        values.sort_unstable();
    }
    values
        .into_iter()
        .map(|n| {
            let title = format!("{n}%");
            let title = if n == 100 {
                t!("settings.default", value = title).into_owned()
            } else {
                title
            };
            Choice::new(n, title, &[])
        })
        .collect()
}

/// Token count options: `presets`, plus `current` if it is not one of them (a value edited in
/// settings.json). `default` is marked.
pub fn token_choices(presets: &[usize], current: usize, default: usize) -> Vec<Choice<usize>> {
    let mut values = presets.to_vec();
    if !values.contains(&current) {
        values.push(current);
        values.sort_unstable();
    }
    values
        .into_iter()
        .map(|n| {
            let title = if n == 0 {
                t!("settings.history_none").into_owned()
            } else {
                t!("settings.tokens", count = n).into_owned()
            };
            let title = if n == default {
                t!("settings.default", value = title).into_owned()
            } else {
                title
            };
            Choice::new(n, title, &[])
        })
        .collect()
}

/// Appearance options.
pub fn theme_choices() -> Vec<Choice<ThemeSetting>> {
    [
        (ThemeSetting::System, "settings.theme_system"),
        (ThemeSetting::Light, "settings.theme_light"),
        (ThemeSetting::Dark, "settings.theme_dark"),
    ]
    .into_iter()
    .map(|(value, key)| Choice::new(value, t!(key).into_owned(), &[]))
    .collect()
}

pub fn style_choices() -> Vec<Choice<StyleChoice>> {
    [
        (StyleChoice::Default, "style.default"),
        (StyleChoice::Polite, "style.polite"),
        (StyleChoice::Plain, "style.plain"),
    ]
    .into_iter()
    .map(|(value, key)| Choice::new(value, t!(key).into_owned(), &[]))
    .collect()
}

/// Index of `value` in `choices`.
pub fn position<V: PartialEq>(choices: &[Choice<V>], value: &V) -> Option<usize> {
    choices.iter().position(|c| &c.value == value)
}

/// A duration such as `45 s`, `3 min 20 s`, or `1 h 05 min`.
pub fn format_duration(d: Duration) -> String {
    let secs = d.as_secs_f64().round() as u64;
    if secs < 60 {
        t!("time.seconds", s = secs).into_owned()
    } else if secs < 3600 {
        t!(
            "time.minutes",
            m = secs / 60,
            s = format!("{:02}", secs % 60)
        )
        .into_owned()
    } else {
        let minutes = secs / 60;
        t!(
            "time.hours",
            h = minutes / 60,
            m = format!("{:02}", minutes % 60)
        )
        .into_owned()
    }
}

/// Format a live count in the UI language.
pub(crate) fn format_count(n: usize) -> String {
    count_for_locale(n, &rust_i18n::locale())
}

fn count_for_locale(n: usize, locale: &str) -> String {
    let separator = match locale {
        "de" | "pt-BR" | "id" => '.',
        "es" if n < 10_000 => return n.to_string(),
        "es" | "fr" => '\u{202f}',
        _ => ',',
    };
    let digits = n.to_string();
    let mut out = String::new();
    for (i, c) in digits.chars().enumerate() {
        if i > 0 && (digits.len() - i).is_multiple_of(3) {
            out.push(separator);
        }
        out.push(c);
    }
    out
}

pub(crate) fn format_decimal(number: String) -> String {
    decimal_for_locale(number, &rust_i18n::locale())
}

pub(crate) fn decimal_for_locale(number: String, locale: &str) -> String {
    match locale {
        "de" | "es" | "fr" | "id" | "pt-BR" => number.replace('.', ","),
        _ => number,
    }
}

pub(crate) const DETECTION_BYTES: usize = 8 * 1024;

fn detection_sample(text: &str) -> Cow<'_, str> {
    // whatlang 0.16.4 counts both of these blocks as Hangul. Ignore width forms
    // and enclosed CJK symbols, but give halfwidth kana a Katakana script vote.
    if text
        .chars()
        .any(|ch| matches!(ch, '\u{3200}'..='\u{32ff}' | '\u{ff00}'..='\u{ffef}'))
    {
        Cow::Owned(
            text.chars()
                .map(|ch| match ch {
                    '\u{ff66}'..='\u{ff9d}' => 'カ',
                    '\u{3200}'..='\u{32ff}' | '\u{ff00}'..='\u{ffef}' => ' ',
                    _ => ch,
                })
                .collect(),
        )
    } else {
        Cow::Borrowed(text)
    }
}

/// Detect against the full language set so an unsupported language cannot win by exclusion.
pub fn detect_source(text: &str) -> Option<&'static str> {
    use whatlang::{Lang, Script};

    let sample = detection_sample(&text[..text.floor_char_boundary(DETECTION_BYTES)]);
    let sample = sample.as_ref();
    let info = whatlang::detect(sample)?;
    let (minimum_letters, minimum_confidence) = match (info.lang(), info.script()) {
        (_, Script::Latin) => (12, 0.5),
        // A kana-bearing result can have Mandarin as its dominant script. Mandarin
        // itself has a separate evidence check below.
        (Lang::Jpn | Lang::Kor | Lang::Cmn, _) => (0, 0.85),
        // Keep the stricter floor for shared scripts, especially Arabic.
        _ => (24, 0.85),
    };
    if sample
        .chars()
        .filter(|ch| ch.is_alphabetic())
        .take(minimum_letters)
        .count()
        < minimum_letters
    {
        return None;
    }
    // The gap between the top two trigram scores can be tiny even for
    // everyday English. Only corroborate an existing English first choice.
    const ENGLISH_WORDS: &[&str] = &[
        "the", "you", "your", "for", "to", "will", "with", "from", "have", "are", "how", "going",
        "would", "could", "should", "this", "that", "they",
    ];
    if info.confidence() < minimum_confidence
        && !(info.lang() == Lang::Eng && has_distinct_words(sample, ENGLISH_WORDS, 4))
    {
        return None;
    }
    // whatlang has no Galician candidate and can call it Portuguese or Spanish.
    const GALICIAN_WORDS: &[&str] = &[
        "calquera",
        "dereito",
        "dereitos",
        "ningunha",
        "ninguén",
        "ninguín",
        "nin",
        "persoa",
        "relixión",
        "súa",
        "súas",
        "teñen",
        "unha",
    ];
    if matches!(info.lang(), Lang::Por | Lang::Spa) && has_distinct_words(sample, GALICIAN_WORDS, 2)
    {
        return None;
    }
    match info.lang() {
        Lang::Jpn if has_japanese_evidence(sample) => Some("ja"),
        whatlang::Lang::Eng => Some("en"),
        Lang::Kor if has_korean_evidence(sample) => Some("ko"),
        whatlang::Lang::Ara => Some("ar"),
        whatlang::Lang::Aze => Some("az"),
        whatlang::Lang::Bel => Some("be"),
        whatlang::Lang::Bul => Some("bg"),
        whatlang::Lang::Ben => Some("bn"),
        whatlang::Lang::Cat => Some("ca"),
        whatlang::Lang::Deu => Some("de"),
        whatlang::Lang::Spa => Some("es"),
        whatlang::Lang::Est => Some("et"),
        whatlang::Lang::Pes => Some("fa"),
        whatlang::Lang::Fra => Some("fr"),
        whatlang::Lang::Guj => Some("gu"),
        whatlang::Lang::Heb => Some("he"),
        whatlang::Lang::Hin => Some("hi"),
        whatlang::Lang::Hrv => Some("hr"),
        whatlang::Lang::Hun => Some("hu"),
        whatlang::Lang::Hye => Some("hy"),
        whatlang::Lang::Ind => Some("id"),
        whatlang::Lang::Ita => Some("it"),
        whatlang::Lang::Jav => Some("jv"),
        whatlang::Lang::Kat => Some("ka"),
        whatlang::Lang::Khm => Some("km"),
        whatlang::Lang::Lit => Some("lt"),
        whatlang::Lang::Lav => Some("lv"),
        whatlang::Lang::Mal => Some("ml"),
        whatlang::Lang::Mar => Some("mr"),
        whatlang::Lang::Mya => Some("my"),
        whatlang::Lang::Nep => Some("ne"),
        whatlang::Lang::Nld => Some("nl"),
        whatlang::Lang::Pan => Some("pa"),
        whatlang::Lang::Pol => Some("pl"),
        whatlang::Lang::Por => Some("pt"),
        whatlang::Lang::Rus => Some("ru"),
        whatlang::Lang::Sin => Some("si"),
        whatlang::Lang::Slk => Some("sk"),
        whatlang::Lang::Slv => Some("sl"),
        whatlang::Lang::Srp => Some("sr"),
        whatlang::Lang::Tam => Some("ta"),
        whatlang::Lang::Tel => Some("te"),
        whatlang::Lang::Tha => Some("th"),
        whatlang::Lang::Tgl => Some("tl"),
        whatlang::Lang::Urd => Some("ur"),
        whatlang::Lang::Uzb => Some("uz"),
        whatlang::Lang::Vie => Some("vi"),
        // The Mandarin script detector also reports traditional Chinese and kanji-only Japanese.
        Lang::Cmn if has_simplified_chinese_evidence(sample) => Some("zh-hans"),
        _ => None,
    }
}

fn is_kana(ch: char) -> bool {
    // Halfwidth kana have already been mapped to fullwidth kana.
    matches!(ch, '\u{3040}'..='\u{309f}' | '\u{30a0}'..='\u{30ff}' | '\u{31f0}'..='\u{31ff}')
        && ch.is_alphabetic()
}

fn is_han(ch: char) -> bool {
    matches!(
        ch,
        '\u{3400}'..='\u{4dbf}'
            | '\u{4e00}'..='\u{9fff}'
            | '\u{f900}'..='\u{faff}'
            | '\u{20000}'..='\u{2fa1f}'
            | '\u{30000}'..='\u{323af}'
    ) && ch.is_alphabetic()
}

// Forms that differ from traditional Chinese and standard Japanese kanji.
const SIMPLIFIED: &str = "严为们关办发变吗图头实应权标检汉测种简级线统经维网联药补观视认让诉试说请读谢购转还进边连递达难电韩页须领风飞饭馆验马鸟齿赋业书问隶气园么这爱习";

fn has_japanese_evidence(text: &str) -> bool {
    // Repeating a simplified form adds no new evidence, as in a name used twice.
    let simplified = simplified_forms(text).count();
    if simplified < 2 {
        return true;
    }
    // Count occurrences of hiragana and Han to judge how much of the sentence is Japanese.
    // Chinese can include katakana names or quote a Japanese phrase.
    let (mut hiragana, mut han, mut quote_depth) = (0, 0, 0usize);
    for ch in text.chars() {
        match ch {
            '「' | '『' | '“' => quote_depth += 1,
            '」' | '』' | '”' => quote_depth = quote_depth.saturating_sub(1),
            '\u{3040}'..='\u{309f}' if quote_depth == 0 && ch.is_alphabetic() => hiragana += 1,
            ch if is_han(ch) => han += 1,
            _ => {}
        }
    }
    hiragana >= 2 * simplified && 2 * hiragana >= han
}

fn has_korean_evidence(text: &str) -> bool {
    let (mut hangul, mut kana, mut han) = (0, 0, 0);
    for ch in text.chars().filter(|ch| ch.is_alphabetic()) {
        match ch {
            '\u{1100}'..='\u{11ff}'
            | '\u{3130}'..='\u{318f}'
            | '\u{a960}'..='\u{a97f}'
            | '\u{ac00}'..='\u{d7ff}' => hangul += 1,
            ch if is_kana(ch) => kana += 1,
            ch if is_han(ch) => han += 1,
            _ => {}
        }
    }
    // Han can be Hanja. If kana is present, require Hangul to clearly outweigh the quote.
    hangul > 0 && (kana == 0 || hangul >= 2 * (kana + han))
}

fn has_simplified_chinese_evidence(text: &str) -> bool {
    const TRADITIONAL: &str = "嚴為們關辦發變嗎圖頭實應權標檢漢測種簡級線統經維網聯藥補觀視認讓訴試說請讀謝購轉還進邊連遞達難電韓頁須領風飛飯館驗馬鳥齒賦業書問隸氣園麼這愛習";
    // Kana (including halfwidth kana mapped for script detection) and common traditional
    // forms, some shared with Japanese, make the simplified Chinese evidence inconclusive.
    const TRADITIONAL_ONLY_IN_CHINESE: &str = "國學體臺灣裡門開見車長東廣點過對會來";
    !looks_japanese(text)
        && !text
            .chars()
            .any(|ch| TRADITIONAL.contains(ch) || TRADITIONAL_ONLY_IN_CHINESE.contains(ch))
        && simplified_forms(text).nth(1).is_some()
}

fn simplified_forms(text: &str) -> impl Iterator<Item = char> + '_ {
    SIMPLIFIED.chars().filter(|&ch| text.contains(ch))
}

fn has_distinct_words(text: &str, words: &[&str], minimum: usize) -> bool {
    words
        .iter()
        .filter(|&&word| {
            text.split(|ch: char| !ch.is_alphabetic()).any(|token| {
                token.eq_ignore_ascii_case(word)
                    || (!word.is_ascii()
                        && token.chars().flat_map(char::to_lowercase).eq(word.chars()))
            })
        })
        .take(minimum)
        .count()
        == minimum
}

pub(crate) fn routed_source(
    profile: ModelProfile,
    requested: SourceChoice,
    detected: Option<&'static str>,
) -> SourceChoice {
    if profile.capabilities().directions.is_some() {
        detected.map_or(requested, SourceChoice::Lang)
    } else {
        requested
    }
}

pub(crate) struct DetectedTextSource {
    pub(crate) code: &'static str,
    text: String,
}

pub(crate) fn discard_detected_source_on_edit(
    previous: &mut Option<DetectedTextSource>,
    text: &str,
    pasted: bool,
) {
    if pasted
        || previous
            .as_ref()
            .is_some_and(|source| !continues_detected_text(&source.text, text))
    {
        *previous = None;
    }
}

fn continues_detected_text(previous: &str, text: &str) -> bool {
    // Ignore nonletters at the edges so a quote or sentence punctuation can wrap the text.
    let previous = previous.trim_matches(|ch: char| !ch.is_alphabetic());
    let text = text.trim_matches(|ch: char| !ch.is_alphabetic());
    if previous == text {
        return !previous.is_empty();
    }
    let (previous_prefix, text_prefix, prefix_letters) = previous
        .chars()
        .zip(text.chars())
        .enumerate()
        .take_while(|&(index, (a, b))| {
            a == b || (index == 0 && a.to_lowercase().eq(b.to_lowercase()))
        })
        .fold(
            (0, 0, 0),
            |(previous_bytes, text_bytes, letters), (_, (a, b))| {
                (
                    previous_bytes + a.len_utf8(),
                    text_bytes + b.len_utf8(),
                    letters + usize::from(a.is_alphabetic()),
                )
            },
        );
    let suffix_letters = previous[previous_prefix..]
        .chars()
        .rev()
        .zip(text[text_prefix..].chars().rev())
        .take_while(|(a, b)| a == b)
        .filter(|(ch, _)| ch.is_alphabetic())
        .count();
    let longest = previous
        .chars()
        .filter(|ch| ch.is_alphabetic())
        .count()
        .max(text.chars().filter(|ch| ch.is_alphabetic()).count());

    // Require a majority of both texts, counting each letter at most once.
    prefix_letters + suffix_letters > longest / 2
}

pub(crate) fn retain_detected_source(
    previous: &mut Option<DetectedTextSource>,
    text: &str,
    detected: Option<&'static str>,
) -> Option<&'static str> {
    if let Some(code) = detected {
        *previous = Some(DetectedTextSource {
            code,
            text: text.to_owned(),
        });
    } else {
        // Keep comparing against the last confident text, even after several ambiguous edits.
        discard_detected_source_on_edit(previous, text, false);
    }
    previous.as_ref().map(|source| source.code)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn formats_counts_and_decimals_by_ui_language() {
        assert_eq!(count_for_locale(0, "en"), "0");
        assert_eq!(count_for_locale(999, "en"), "999");
        assert_eq!(count_for_locale(1000, "ja"), "1,000");
        assert_eq!(count_for_locale(1234567, "en"), "1,234,567");
        assert_eq!(count_for_locale(1000, "es"), "1000");
        assert_eq!(count_for_locale(12345, "fr"), "12\u{202f}345");
        assert_eq!(count_for_locale(12345, "de"), "12.345");
        assert_eq!(decimal_for_locale("1.80".into(), "pt-BR"), "1,80");
        assert_eq!(decimal_for_locale("1.80".into(), "zh-Hant"), "1.80");
    }

    #[test]
    fn model_decimals_keep_precision_and_sign_in_each_locale() {
        for locale in ["de", "es", "fr", "id", "pt-BR"] {
            assert_eq!(decimal_for_locale("0.580".into(), locale), "0,580");
            assert_eq!(decimal_for_locale("-0.006".into(), locale), "-0,006");
            assert_eq!(decimal_for_locale("+0.0".into(), locale), "+0,0");
        }
        for locale in ["en", "ja"] {
            assert_eq!(decimal_for_locale("0.580".into(), locale), "0.580");
            assert_eq!(decimal_for_locale("-0.006".into(), locale), "-0.006");
            assert_eq!(decimal_for_locale("+0.0".into(), locale), "+0.0");
        }
    }

    #[test]
    fn ui_catalogs_match_english_keys_and_placeholders() {
        fn placeholders(text: &str) -> Vec<&str> {
            let mut names: Vec<_> = text
                .split("%{")
                .skip(1)
                .map(|part| part.split_once('}').expect("unclosed placeholder").0)
                .collect();
            names.sort_unstable();
            names
        }

        let backend = crate::_rust_i18n_backend();
        let english = backend.messages_for_locale("en").unwrap();
        for &(_, locale, _) in UiLanguage::OPTIONS {
            let messages: std::collections::HashMap<_, _> = backend
                .messages_for_locale(locale)
                .unwrap_or_else(|| panic!("missing catalog: {locale}"))
                .into_iter()
                .collect();
            for (key, source) in &english {
                let value = messages
                    .get(key)
                    .unwrap_or_else(|| panic!("missing {locale}: {key}"));
                assert!(!value.trim().is_empty(), "empty {locale}: {key}");
                assert_eq!(placeholders(value), placeholders(source), "{locale}: {key}");
            }
            assert_eq!(messages.len(), english.len(), "{locale}: unexpected keys");
        }
    }

    #[test]
    fn detects_the_initial_trial_samples_and_long_french() {
        assert_eq!(
            detect_source(sample_source(Some("en")).unwrap()),
            Some("en")
        );
        assert_eq!(
            detect_source(sample_source(Some("ja")).unwrap()),
            Some("ja")
        );
        assert_eq!(
            detect_source(
                "La bibliothèque est fermée le dimanche. Vous pouvez retourner les livres lundi ou utiliser la boîte près de la porte."
            ),
            Some("fr")
        );
    }

    #[test]
    fn pinning_keeps_supported_pairs_and_excludes_portuguese_for_plamo() {
        let mut settings = Settings {
            target_language: TargetChoice::Lang("fr"),
            ..Settings::default()
        };
        let mut languages = Languages {
            automatic: true,
            source: SourceChoice::Lang("en"),
            ..Languages::default()
        };
        assert!(!languages.pin(ModelProfile::HyMt2, &mut settings));
        languages.load(ModelProfile::HyMt2, &settings);
        assert_eq!(languages.target(&settings), TargetChoice::Lang("fr"));
        assert_eq!(languages.source, SourceChoice::Lang("en"));
        languages.choose_target(TargetChoice::Lang("pt"), &mut settings);
        assert!(languages.pin(ModelProfile::PlamoPfml, &mut settings));
        assert_eq!(languages.source, SourceChoice::Lang("en"));
        assert!(
            languages
                .target_choices()
                .iter()
                .all(|choice| choice.value != TargetChoice::Lang("pt"))
        );
        assert_ne!(languages.target(&settings), TargetChoice::Lang("pt"));
    }

    #[test]
    fn detects_short_messages() {
        for (text, code) in [
            ("ありがとうございます。", "ja"),
            ("会議は木曜日の午後三時に変更になりました。", "ja"),
            ("最新版の契約書を送っていただけますか？", "ja"),
            ("このボタンを押すと保存されます。", "ja"),
            ("감사합니다.", "ko"),
            ("회의가 목요일 오후 세 시로 변경되었습니다.", "ko"),
            ("Saya tidak mengerti.", "id"),
            ("Eu não entendo.", "pt"),
            (
                "Thank you for your email. I will get back to you tomorrow.",
                "en",
            ),
            (
                "Hello, how are you? I hope everything is going well at your new job.",
                "en",
            ),
            (
                "Semua orang dilahirkan merdeka dan mempunyai martabat dan hak-hak yang sama.",
                "id",
            ),
        ] {
            assert_eq!(detect_source(text), Some(code), "{text}");
        }
    }

    #[test]
    fn detects_new_hy_mt2_sources_but_leaves_plamo_detection_to_the_model() {
        for code in ["zh-hans", "es", "ko"] {
            let detected = detect_source(sample_source(Some(code)).unwrap());
            assert_eq!(detected, Some(code));
            assert_eq!(
                routed_source(ModelProfile::HyMt2, SourceChoice::Detect, detected),
                SourceChoice::Lang(code),
            );
            assert_eq!(
                routed_source(ModelProfile::PlamoPfml, SourceChoice::Detect, detected),
                SourceChoice::Detect,
            );
        }
    }

    #[test]
    fn fullwidth_forms_and_halfwidth_kana_do_not_trigger_korean_detection() {
        for (text, expected) in [
            ("ＩＤ：ＡＢＣ１２３ ご確認ください", Some("ja")),
            ("会員番号：ＡＢ１２３４５６７", None),
            ("ＴＥＬ：０３－１２３４－５６７８ 担当：山田", None),
            ("ｱﾘｶﾞﾄｳｺﾞｻﾞｲﾏｽ", Some("ja")),
            ("ＴＨＡＮＫ ＹＯＵ", None),
            ("㍿㍑㌔", None),
            ("これは日本語です。 안녕하세요안녕하세요", None),
            (
                "番号：ＡＢＣ１２３４５６７８９０ 请问图书馆在哪里？",
                Some("zh-hans"),
            ),
            (
                "El informe de la comisión se publicará antes de finales de este mes. ＡＢＣＤＥＦＧＨＩＪＫＬＭＮＯＰＱＲＳＴＵＶＷＸＹＺ１２３４５６７８９０１２３４５６７８９０１２３４５６７８９０１２３４５６７８９０１２３４５６７８９０。",
                Some("es"),
            ),
            ("Hello ＡＢＣＤＥＦＧ", None),
            ("안녕하세요！１２３４５６７８９０", Some("ko")),
            ("안녕", Some("ko")),
            ("ㅎㅇ", Some("ko")),
            ("\u{a960}\u{d7b0}", Some("ko")),
        ] {
            assert_eq!(detect_source(text), expected, "{text}");
        }
    }

    #[test]
    fn simplified_chinese_detection_requires_unambiguous_script_evidence() {
        for text in [
            "明天下午我们在图书馆见面，然后一起去喝茶。",
            "你好，请问图书馆在哪里？",
            "我爱学习中文。",
        ] {
            assert_eq!(detect_source(text), Some("zh-hans"));
        }
        for text in [
            "東京大学",
            "国際連合安全保障理事会",
            "人人平等",
            "請問這間圖書館在哪裡？",
            "请問这间图書館在哪里？",
            "请问臺灣在哪裡？",
            "请问怎么走 ｶﾀｶﾅ",
        ] {
            assert_eq!(detect_source(text), None);
        }
    }

    #[test]
    fn kana_and_hangul_keep_their_own_source_languages() {
        assert_eq!(detect_source("東京駅で乗り換えてください。"), Some("ja"));
        assert_eq!(detect_source("안녕하세요"), Some("ko"));
        assert_eq!(detect_source("韓國 서울에서 출발합니다."), Some("ko"));
    }

    #[test]
    fn japanese_can_quote_simplified_chinese() {
        for japanese in [
            "中国語の「图书馆」は、本を借りられる場所のことです。",
            "「图书馆」とは本を借りる場所です。",
            "华为は新型端末を発表した。华为の社長が説明。",
        ] {
            assert_eq!(detect_source(japanese), Some("ja"), "{japanese}");
        }
        for chinese in [
            "明天下午我们在图书馆见面，然后一起去喝茶。のかなです",
            "明天下午我们在公园一起喝茶。のかなです",
        ] {
            assert_eq!(detect_source(chinese), None, "{chinese}");
        }
    }

    #[test]
    fn chinese_with_japanese_names_or_greetings_stays_inconclusive() {
        for chinese in [
            "这个プレイステーション游戏很好玩，我们都喜欢。",
            "我们这次去日本买了很多ポッキー和キットカット。",
            "我们在ドン・キホーテ买了很多东西，还有药。",
            "《ドラゴンボール》这部动画我们小时候都看过。",
            "「ファイナルファンタジー」这个游戏我们玩过。",
            "他说「おはようございます」，我们也说早上好。",
        ] {
            assert_eq!(detect_source(chinese), None, "{chinese}");
        }
    }

    #[test]
    fn katakana_heavy_japanese_quoting_simplified_forms_can_be_inconclusive() {
        for japanese in [
            "「图书馆」でデジタルアーカイブのイベント。",
            "「图书馆」デジタルアーカイブ・サービス。",
        ] {
            assert_eq!(detect_source(japanese), None, "{japanese}");
        }
        assert_eq!(detect_source("デジタルアーカイブ・サービス。"), Some("ja"));
    }

    #[test]
    fn korean_can_quote_a_little_japanese() {
        let korean = "친구에게 일본어 인사말인 「おはよう」를 알려 주었습니다.";
        let mixed = "일본어로 아침 인사를 합니다. 「おはよう、元気ですか」";
        assert_eq!(detect_source(mixed), None);
        assert_eq!(detect_source(korean), Some("ko"));
    }

    #[test]
    fn distinguishes_spanish_from_neighboring_languages() {
        for (text, expected) in [
            (
                "El informe de la comisión se publicará antes de finales de este mes.",
                Some("es"),
            ),
            (
                "¿Puede decirme dónde están las llaves de la puerta del jardín?",
                Some("es"),
            ),
            (
                "A miña persoa ten dereito á educación e ningunha lei o nega.",
                None,
            ),
            (
                "O hospital precisa contratar mais médicos antes do início do próximo verão.",
                Some("pt"),
            ),
            (
                "La biblioteca estarà tancada els diumenges durant les vacances d'estiu.",
                Some("ca"),
            ),
            (
                "La biblioteca rimarrà chiusa la domenica durante tutte le vacanze estive.",
                None,
            ),
            (
                "Las entradas para este concierto se agotaron hace varias semanas.",
                None,
            ),
        ] {
            assert_eq!(detect_source(text), expected, "{text}");
        }
    }

    #[test]
    fn routing_detection_requires_evidence() {
        for text in ["Thank you", "漢字", "谢谢", "Спасибо", "123"] {
            assert_eq!(detect_source(text), None, "{text}");
        }
        // Article 1 of the Universal Declaration of Human Rights.
        for text in [
            "Tutti gli esseri umani nascono liberi ed eguali in dignità e diritti.",
            "Alle mensen worden vrij en gelijk in waardigheid en rechten geboren.",
            "Tots els éssers humans neixen lliures i iguals en dignitat i en drets.",
            "Todo individuo ten dereito á vida, á liberdade e á seguridade da súa persoa.",
            "Ninguín será sometido a torturas nin a penas ou tratos crueis, inhumanos e aldraxantes.",
            "Alle menslike wesens word vry, met gelyke waardigheid en regte, gebore.",
            "Toate ființele umane se nasc libere și egale în demnitate și în drepturi.",
            "Alle mennesker er født frie og lige i værdighed og rettigheder.",
            "Alla människor är födda fria och lika i värde och rättigheter.",
            "تمام افراد بشر آزاد به دنیا می‌آیند و از لحاظ حیثیت و حقوق با هم برابرند.",
            "تمام انسان آزاد اور حقوق و عزت کے اعتبار سے برابر پیدا ہوئے ہیں۔",
        ] {
            if let Some(code) = detect_source(text) {
                for profile in [ModelProfile::HyMt2, ModelProfile::IndexTranslate] {
                    assert!(
                        !engine::selection::supports_direction(profile, Some(code), Some("en")),
                        "{text}: {code}"
                    );
                }
            }
        }
    }

    #[test]
    fn inconclusive_edits_switch_to_hy_mt2_at_most_once() {
        use engine::{
            models,
            selection::{self, QualityTier, Request},
        };

        let entries = models::entries();
        let plamo = entries
            .iter()
            .copied()
            .find(|entry| entry.is_plamo() && entry.size == "2b")
            .unwrap();
        let hy = models::HY_MT2.id();
        let installed = [hy, plamo.id()];
        for (from, to, text, backspaced) in [
            (
                "en",
                "fr",
                "We will inspect the bridge tomorrow",
                "We will inspect the bridge",
            ),
            (
                "es",
                "en",
                "Mañana revisaremos el puente nuevo",
                "Mañana revisaremos el puente",
            ),
            (
                "ko",
                "en",
                "내일 새 다리를 다시 점검합니다",
                "내일 새 다리를 다시",
            ),
            ("zh-hans", "en", "我们明天会检查这座新桥", "我们明天会检查"),
        ] {
            let mut previous = None;
            let appended = format!("{text} (1)");
            let sequence = [
                ("1", None),
                ("12", None),
                (text, Some(from)),
                (&appended, None),
                (text, Some(from)),
                (backspaced, None),
            ];
            let routed = sequence.map(|(text, detected)| {
                let source = retain_detected_source(&mut previous, text, detected);
                selection::select(
                    Request {
                        source,
                        target: Some(to),
                        format: None,
                        system_memory: 0,
                        quality: QualityTier::Standard,
                        plamo_consent: true,
                        bidirectional: false,
                        selected: None,
                    },
                    &entries,
                    &installed,
                )
                .usable
                .unwrap()
                .id()
            });
            assert_eq!(routed, [plamo.id(), plamo.id(), hy, hy, hy, hy], "{from}");
        }
    }

    #[test]
    fn source_continuity_requires_a_majority_of_letters_in_both_texts() {
        let original = "mañana revisamos el plano del jardín";
        for continued in [
            original,
            "mañana revisamos el plano del",
            "mañana revisamos el plano del jardín temprano",
            "hoy por la tarde, mañana revisamos el plano del jardín",
            "mañana revisamos el mapa del jardín",
        ] {
            assert!(continues_detected_text(original, continued), "{continued}");
        }
        for replaced in [
            "mañana",
            "del jardín",
            "東京駅で乗り換えてください。",
            "",
            "   ",
        ] {
            assert!(!continues_detected_text(original, replaced), "{replaced}");
        }
        assert!(!continues_detected_text("ab", "ababab"));
        assert!(!continues_detected_text("a 123456789", "b 123456789"));
    }

    #[test]
    fn wrapping_or_capitalizing_and_punctuating_retains_the_source() {
        let original = "ángela revisará el plano del jardín";
        for edited in [
            "«ángela revisará el plano del jardín»",
            "Ángela revisará el plano del jardín.",
            "“Ángela revisará el plano del jardín.”",
        ] {
            let mut previous = None;
            retain_detected_source(&mut previous, original, Some("es"));
            assert_eq!(
                retain_detected_source(&mut previous, edited, None),
                Some("es")
            );
        }
        // These case-equivalent initial letters occupy different numbers of bytes.
        assert!(continues_detected_text(
            "Kelvin revisa el plano",
            "kelvin revisó el plano.",
        ));
    }

    #[test]
    fn per_edit_discard_does_not_resurrect_a_source_before_translation() {
        let spanish = "El informe de la comisión se publicará antes de finales de este mes.";
        let italian = "La biblioteca rimarrà chiusa la domenica durante tutte le vacanze estive.";
        let related = "El informe de la comisión se publicará antes de finales de este";
        let mut previous = None;

        for (replacement, pasted) in [(italian, false), ("", false), (related, true)] {
            retain_detected_source(&mut previous, spanish, detect_source(spanish));
            discard_detected_source_on_edit(&mut previous, related, false);
            assert!(previous.is_some());
            // Both changes arrive before the translation debounce expires.
            discard_detected_source_on_edit(&mut previous, replacement, pasted);
            discard_detected_source_on_edit(&mut previous, related, false);
            assert_eq!(retain_detected_source(&mut previous, related, None), None);
        }
    }

    #[test]
    fn inconclusive_italian_replacement_drops_spanish_source() {
        let spanish = "El informe de la comisión se publicará antes de finales de este mes.";
        let italian = "La biblioteca rimarrà chiusa la domenica durante tutte le vacanze estive.";
        let mut previous = None;
        assert_eq!(
            retain_detected_source(&mut previous, spanish, detect_source(spanish)),
            Some("es"),
        );
        assert_eq!(detect_source(italian), None);
        let source = retain_detected_source(&mut previous, italian, detect_source(italian));
        assert_eq!(source, None);
        assert_eq!(
            routed_source(ModelProfile::HyMt2, SourceChoice::Detect, source),
            SourceChoice::Detect,
        );
    }

    #[test]
    fn ambiguous_edits_keep_the_confident_text_as_the_anchor() {
        let mut previous = None;
        assert_eq!(
            retain_detected_source(
                &mut previous,
                "고양이는 따뜻한 방에서 잠을 잡니다",
                Some("ko")
            ),
            Some("ko"),
        );
        let edit = "고양이는 따뜻한 방에서";
        assert_eq!(
            retain_detected_source(&mut previous, edit, None),
            Some("ko")
        );
        // Each cut is small relative to the previous input, but together they leave a fragment.
        assert_eq!(
            retain_detected_source(&mut previous, "고양이는 따뜻한", None),
            None
        );
        assert_eq!(retain_detected_source(&mut previous, edit, None), None);
    }

    #[test]
    fn confident_edits_update_the_retained_source() {
        let mut previous = None;
        let spanish = "Mañana revisamos el plano del jardín.";
        let korean = "내일 새 다리를 점검하겠습니다.";
        assert_eq!(
            retain_detected_source(&mut previous, spanish, Some("es")),
            Some("es")
        );
        assert_eq!(
            retain_detected_source(&mut previous, korean, Some("ko")),
            Some("ko")
        );
        assert_eq!(
            retain_detected_source(&mut previous, "내일 새 다리를 점검", None),
            Some("ko")
        );
    }

    #[test]
    fn unknown_source_needs_a_model_with_detection() {
        use engine::{
            models,
            selection::{self, QualityTier, Request},
        };

        let entries = models::entries();
        let plamo = entries
            .iter()
            .copied()
            .find(|entry| entry.is_plamo() && entry.size == "2b")
            .unwrap();
        let hy = models::HY_MT2.id();
        let request = Request {
            source: detect_source("漢字"),
            target: Some("fr"),
            format: None,
            system_memory: 0,
            quality: QualityTier::Standard,
            plamo_consent: true,
            bidirectional: false,
            selected: None,
        };
        assert!(selection::select(request, &entries, &[hy]).usable.is_none());
        assert!(
            selection::select(
                Request {
                    plamo_consent: false,
                    ..request
                },
                &entries,
                &[hy, plamo.id()],
            )
            .usable
            .is_none()
        );
        assert_eq!(
            selection::select(request, &entries, &[hy, plamo.id()])
                .usable
                .unwrap()
                .id(),
            plamo.id()
        );
        assert_eq!(
            routed_source(plamo.profile, SourceChoice::Detect, None),
            SourceChoice::Detect
        );
        assert_eq!(
            routed_source(plamo.profile, SourceChoice::Detect, Some("en")),
            SourceChoice::Detect
        );
        assert_eq!(
            routed_source(ModelProfile::HyMt2, SourceChoice::Detect, Some("en")),
            SourceChoice::Lang("en")
        );
    }

    #[test]
    fn routes_detected_english_to_an_installed_model_for_each_target() {
        use engine::{
            models::{self, HY_MT2},
            selection::{self, Request},
        };
        let catalog = models::entries();
        let plamo = catalog
            .iter()
            .copied()
            .find(|entry| entry.is_plamo() && entry.size == "2b" && entry.quant == "Q4_K_M")
            .unwrap();
        let source =
            super::detect_source("All human beings are born free and equal in dignity and rights.");
        for (target, model) in [("ja", plamo.id()), ("fr", HY_MT2.id())] {
            let selected = selection::select(
                Request {
                    source,
                    target: Some(target),
                    format: None,
                    system_memory: 0,
                    quality: selection::QualityTier::Standard,
                    plamo_consent: true,
                    bidirectional: false,
                    selected: None,
                },
                &catalog,
                &[plamo.id(), HY_MT2.id()],
            );
            assert_eq!(selected.usable.unwrap().id(), model);
        }
    }

    #[test]
    fn detects_japanese_by_kana() {
        assert!(looks_japanese("会議は来週です"));
        assert!(looks_japanese("カタカナ"));
        assert!(!looks_japanese("Hello, world"));
        // Kanji only: could be Chinese.
        assert!(!looks_japanese("你好世界"));
    }

    /// Serializes the tests that change the process-wide locale.
    use super::TEST_LOCALE as LOCALE;

    #[test]
    fn formats_durations() {
        let _locale = LOCALE.lock().unwrap_or_else(|e| e.into_inner());
        rust_i18n::set_locale("en");
        assert_eq!(format_duration(Duration::from_secs(45)), "45 s");
        assert_eq!(format_duration(Duration::from_secs(75)), "1 min 15 s");
        assert_eq!(format_duration(Duration::from_secs(3725)), "1 h 02 min");
    }

    #[test]
    fn japanese_english_pair() {
        assert_eq!(
            TargetChoice::JapaneseEnglish.resolve_for(looks_japanese("こんにちは")),
            "en"
        );
        assert_eq!(
            TargetChoice::JapaneseEnglish.resolve_for(looks_japanese("Hello")),
            "ja"
        );
        assert_eq!(
            TargetChoice::JapaneseEnglish.resolve_for(looks_japanese("你好")),
            "ja"
        );
        assert_eq!(
            TargetChoice::Lang("fr").resolve_for(looks_japanese("こんにちは")),
            "fr"
        );
    }

    #[test]
    fn every_language_has_a_localized_name() {
        let _locale = LOCALE.lock().unwrap_or_else(|e| e.into_inner());
        for locale in ["en", "ja"] {
            rust_i18n::set_locale(locale);
            for l in LANGUAGES {
                let key = format!("lang.{}", l.code.replace('-', "_"));
                assert_ne!(t!(&key), key, "missing {key} for {locale}");
            }
            assert_ne!(language_name(JA_EASY), JA_EASY);
            // The external models' languages, Portuguese among them, which PLaMo does not list.
            for profile in [ModelProfile::HyMt2, ModelProfile::IndexTranslate] {
                for &(from, to) in profile.capabilities().directions.unwrap() {
                    for code in [from, to] {
                        let key = format!("lang.{}", code.replace('-', "_"));
                        assert_ne!(t!(&key), key, "missing {key} for {locale}");
                    }
                }
            }
        }
        rust_i18n::set_locale("en");
    }

    use SourceChoice::{Detect, Lang as From};
    use TargetChoice::{JapaneseEnglish, Lang as To};

    const EXTERNAL: [ModelProfile; 2] = [ModelProfile::HyMt2, ModelProfile::IndexTranslate];

    fn pair(source: &str, target: &str) -> Option<LanguagePair> {
        Some(LanguagePair {
            source: source.into(),
            target: target.into(),
        })
    }

    fn loaded(profile: ModelProfile, settings: &Settings) -> Languages {
        let mut languages = Languages::default();
        languages.load(profile, settings);
        languages
    }

    #[test]
    fn an_external_model_gets_back_its_saved_pair_or_its_first() {
        for profile in EXTERNAL {
            for (saved, expected) in [
                (None, (From("en"), To("ja"))),
                (pair("ja", "en"), (From("ja"), To("en"))),
                (pair("en", "pt"), (From("en"), To("pt"))),
                (pair("ar", "en"), (From("ar"), To("en"))),
                // Pairs the model does not translate.
                (pair("en", "zh-hant"), (From("en"), To("ja"))),
                (pair("fr", "de"), (From("en"), To("ja"))),
            ] {
                let settings = Settings {
                    external_languages: saved,
                    ..Settings::default()
                };
                let languages = loaded(profile, &settings);
                assert_eq!(
                    (languages.source, languages.target(&settings)),
                    expected,
                    "{profile:?}"
                );
            }
        }
    }

    #[test]
    fn hy_mt2_language_choices_name_each_new_language_in_both_locales() {
        let _locale = LOCALE.lock().unwrap_or_else(|e| e.into_inner());
        let settings = Settings::default();
        let profile = ModelProfile::HyMt2;
        let languages = loaded(profile, &settings);
        for (locale, names) in [
            ("en", ["Chinese (Simplified)", "Spanish", "Korean"]),
            ("ja", ["中国語（簡体字）", "スペイン語", "韓国語"]),
        ] {
            rust_i18n::set_locale(locale);
            let sources = languages.source_choices();
            let targets = languages.target_choices();
            for (code, name) in ["zh-hans", "es", "ko"].into_iter().zip(names) {
                let source = sources
                    .iter()
                    .find(|choice| choice.value == From(code))
                    .unwrap();
                assert_eq!(source.title(), name);
                let target = targets
                    .iter()
                    .find(|choice| choice.value == To(code))
                    .unwrap();
                assert_eq!(target.title(), name);
            }
        }
        rust_i18n::set_locale("en");
    }

    #[test]
    fn choices_with_an_external_model_save_only_its_pair() {
        for profile in EXTERNAL {
            let mut settings = Settings {
                target_language: To("ko"),
                ..Settings::default()
            };
            let mut languages = loaded(profile, &settings);
            assert!(languages.choose_source(From("ja"), &mut settings));
            // Japanese goes to English only.
            assert_eq!(languages.target(&settings), To("en"));
            assert_eq!(settings.external_languages, pair("ja", "en"));
            // Neither detection, nor a target the source does not reach.
            assert!(!languages.choose_source(Detect, &mut settings));
            assert!(!languages.choose_target(To("fr"), &mut settings));
            assert!(!languages.choose_target(JapaneseEnglish, &mut settings));
            assert!(languages.swap(&mut settings));
            assert_eq!(settings.external_languages, pair("en", "ja"));
            assert!(languages.choose_target(To("ar"), &mut settings));
            assert_eq!(settings.external_languages, pair("en", "ar"));
            assert_eq!(
                languages.target_choices().len(),
                profile_targets(profile, From("en")).count()
            );
            assert_eq!(settings.target_language, To("ko"), "PLaMo's target stays");
        }
    }

    #[test]
    fn plamo_choices_leave_the_external_pair_alone() {
        let mut settings = Settings {
            external_languages: pair("en", "pt"),
            target_language: JapaneseEnglish,
            ..Settings::default()
        };
        let mut languages = loaded(ModelProfile::PlamoPfml, &settings);
        assert_eq!(
            (languages.source, languages.target(&settings)),
            (Detect, JapaneseEnglish)
        );
        // As before: the translation can move into the source, the languages stay.
        assert!(languages.can_swap(&settings));
        assert!(!languages.swap(&mut settings));
        assert!(!languages.choose_source(From("ko"), &mut settings));
        assert!(languages.choose_target(To("fr"), &mut settings));
        assert!(languages.swap(&mut settings));
        assert_eq!(
            (languages.source, settings.target_language),
            (From("fr"), To("ko"))
        );
        assert_eq!(settings.external_languages, pair("en", "pt"));
        // Back and forth: PLaMo gets its own source back.
        languages.load(ModelProfile::HyMt2, &settings);
        assert_eq!(
            (languages.source, languages.target(&settings)),
            (From("en"), To("pt"))
        );
        languages.load(ModelProfile::IndexTranslate, &settings);
        languages.load(ModelProfile::PlamoPfml, &settings);
        assert_eq!(
            (languages.source, languages.target(&settings)),
            (From("fr"), To("ko"))
        );
    }

    /// With Hy-MT2 and Japanese → English chosen, a restart (source back to detection)
    /// overwrote the saved target with Japanese.
    #[test]
    fn a_restart_gets_back_the_external_pair_without_saving() {
        let mut settings = Settings {
            target_language: JapaneseEnglish,
            ..Settings::default()
        };
        let mut languages = loaded(ModelProfile::HyMt2, &settings);
        assert!(languages.choose_source(From("ja"), &mut settings));
        let saved = serde_json::to_string(&settings).unwrap();
        let restarted: Settings = serde_json::from_str(&saved).unwrap();
        let languages = loaded(ModelProfile::HyMt2, &restarted);
        assert_eq!(
            (languages.source, languages.target(&restarted)),
            (From("ja"), To("en"))
        );
        assert_eq!(serde_json::to_string(&restarted).unwrap(), saved);
        assert_eq!(restarted.target_language, JapaneseEnglish);
    }

    /// If a damaged Hy-MT2 model falls back to PLaMo, preserve the saved Portuguese choice.
    /// Restoring Hy-MT2 must recover that choice after the repair.
    #[test]
    fn a_fallback_to_plamo_keeps_the_external_pair_for_after_the_repair() {
        let settings: Settings = serde_json::from_value(serde_json::json!({
            "model": "tencent/hy-mt2-1.8b/q4_k_m@a0c709d9fac510f2c807aa3af52872340dc37a4a",
            "target_language": "fr",
            "external_languages": {"source": "en", "target": "pt"},
        }))
        .unwrap();
        let before = settings.clone();
        let mut languages = Languages::default();
        // The damaged file is rejected and PLaMo loads instead.
        languages.load(ModelProfile::PlamoPfml, &settings);
        assert_eq!(
            (languages.source, languages.target(&settings)),
            (Detect, To("fr"))
        );
        // After the repair.
        languages.load(ModelProfile::HyMt2, &settings);
        assert_eq!(
            (languages.source, languages.target(&settings)),
            (From("en"), To("pt"))
        );
        assert_eq!(settings, before);
    }
}
