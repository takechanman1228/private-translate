//! Persistent settings and application directories.

use std::io::{self, Write};
use std::path::{Path, PathBuf};

use engine::document::DocumentOptions;
use engine::pfml::{Form, JA_EASY, find_language};
use serde::{Deserialize, Serialize};

fn project_dirs() -> Option<directories::ProjectDirs> {
    directories::ProjectDirs::from(
        "",
        "",
        if local_preview() {
            "private-translate-preview"
        } else {
            "private-translate"
        },
    )
}

/// The only legacy profile used for settings and model migration.
const LEGACY_PREVIEW: &str = "plamo-translate-desktop-preview";

/// Import the trial profile. An isolated profile only considers its explicit fake source.
pub fn migrate_legacy_settings() {
    let Some(destination) = settings_path() else {
        return;
    };
    let sources: Vec<_> = if local_preview() {
        std::env::var_os("PRIVATE_TRANSLATE_PREVIEW_LEGACY_DIR")
            .map(PathBuf::from)
            .filter(|dir| preview_data_dir().is_some() && dir.is_absolute())
            .map(|dir| (dir.join("settings.json"), dir.join("models")))
            .into_iter()
            .collect()
    } else {
        [LEGACY_PREVIEW]
            .into_iter()
            .filter_map(|name| directories::ProjectDirs::from("", "", name))
            .map(|dirs| {
                (
                    dirs.config_dir().join("settings.json"),
                    dirs.data_local_dir().join("models"),
                )
            })
            .collect()
    };
    if let Err(error) = import_settings(&destination, &default_models_dir(), &sources) {
        crate::app_log!("could not import legacy settings: {error}");
    }
}

fn import_settings(
    destination: &Path,
    new_models: &Path,
    sources: &[(PathBuf, PathBuf)],
) -> io::Result<bool> {
    match destination.symlink_metadata() {
        Ok(_) => return Ok(false),
        Err(error) if error.kind() == io::ErrorKind::NotFound => {}
        Err(error) => return Err(error),
    }
    for (source, old_models) in sources {
        let bytes = match std::fs::read(source) {
            Ok(bytes) => bytes,
            Err(error) if error.kind() == io::ErrorKind::NotFound => continue,
            Err(error) => return Err(error),
        };
        let bytes = bytes.strip_prefix(b"\xEF\xBB\xBF").unwrap_or(&bytes);
        let mut document: serde_json::Value = serde_json::from_slice(bytes)?;
        let settings: Settings = serde_json::from_value(document.clone())?;
        // The trial forced update checks off. Let the release profile use its own default.
        let removed_update_preference = document
            .as_object_mut()
            .and_then(|fields| fields.remove("check_updates"))
            .is_some();
        let reuse_old_models = settings.models_dir.is_none()
            && old_models.is_dir()
            && matches!(new_models.symlink_metadata(), Err(e) if e.kind() == io::ErrorKind::NotFound);
        if reuse_old_models {
            document["models_dir"] = serde_json::to_value(old_models)?;
        }
        let imported = if removed_update_preference || reuse_old_models {
            serde_json::to_vec_pretty(&document)?
        } else {
            bytes.to_vec()
        };
        let parent = destination
            .parent()
            .expect("settings have a parent directory");
        std::fs::create_dir_all(parent)?;
        let mut copy = tempfile::NamedTempFile::new_in(parent)?;
        copy.write_all(&imported)?;
        return match copy.persist_noclobber(destination) {
            Ok(_) => Ok(true),
            Err(error) if error.error.kind() == io::ErrorKind::AlreadyExists => Ok(false),
            Err(error) => Err(error.error),
        };
    }
    Ok(false)
}

/// Opt-in local testing profile: separate settings, logs and single-instance lock.
pub fn local_preview() -> bool {
    std::env::var_os("PRIVATE_TRANSLATE_LOCAL_PREVIEW").is_some_and(|value| value == "1")
}

/// Give a local preview its own settings and models for an acceptance test.
fn preview_data_dir() -> Option<PathBuf> {
    local_preview()
        .then(|| std::env::var_os("PRIVATE_TRANSLATE_PREVIEW_DATA_DIR").map(PathBuf::from))
        .flatten()
        .filter(|path| path.is_absolute())
}

/// The data directory: the default models folder, caches and logs.
fn data_dir() -> Option<PathBuf> {
    preview_data_dir().or_else(|| project_dirs().map(|dirs| dirs.data_local_dir().to_path_buf()))
}

/// Where models are kept unless the settings name another folder (see [`Settings::models_dir`]).
pub fn default_models_dir() -> PathBuf {
    data_dir().map_or_else(|| PathBuf::from("models"), |dir| dir.join("models"))
}

/// Catalog entries found on Hugging Face, cached for offline starts.
pub fn catalog_cache_path() -> PathBuf {
    data_file("catalog.json")
}

/// What the last check for a newer release found (see the `update` crate).
pub fn update_check_path() -> PathBuf {
    data_file("update.json")
}

/// The application log, next to the models.
pub fn log_path() -> PathBuf {
    data_file("app.log")
}

/// A file in the data directory.
pub fn data_file(name: &str) -> PathBuf {
    data_dir().map_or_else(|| PathBuf::from(name), |dir| dir.join(name))
}

/// Where the settings are saved.
pub fn settings_path() -> Option<PathBuf> {
    preview_data_dir()
        .or_else(|| project_dirs().map(|dirs| dirs.config_dir().to_path_buf()))
        .map(|dir| dir.join("settings.json"))
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct Settings {
    /// New profiles may resume their initial preparation across launches.
    /// Settings saved by versions before onboarding already belong to a returning user.
    #[serde(default)]
    pub first_run_setup_pending: bool,
    /// Catalog id of the selected model.
    pub model: Option<String>,
    pub model_fixed: bool,
    pub model_high_quality: bool,
    /// The folder models are downloaded to and loaded from, if not the default one.
    pub models_dir: Option<PathBuf>,
    /// Whether the user accepted the PLaMo Community License before downloading.
    pub license_accepted: bool,
    pub backend: BackendSettings,
    /// Glossary text, one `source = target` per line.
    pub glossary: String,
    pub ui_language: UiLanguage,
    pub theme: ThemeSetting,
    /// Size of the interface in percent, on top of the scaling the OS applies (some Linux
    /// desktops apply none on a high-DPI display).
    pub ui_scale: u16,
    pub document: DocumentSettings,
    /// Scroll the source and the translation together, and show the counterpart of the caret.
    pub sync_scroll: bool,
    /// Ask GitHub for a newer release, at most once a day.
    pub check_updates: bool,
    /// Translate the clipboard when it is copied to twice in quick succession, in any
    /// application (Ctrl+C or ⌘C pressed twice).
    pub double_copy: bool,
    /// Keep running when the window is closed: the notification-area icon (Windows), the Dock
    /// (macOS), or starting the app again opens it.
    pub resident: bool,
    /// PDF layout export is opt-in; a bool stays readable by older versions.
    pub pdf_layout: bool,
    /// The external `babeldoc-engine` executable the user chose, if any (macOS only). Nothing
    /// is installed with the app; a value of another shape reads as none.
    #[serde(deserialize_with = "lenient")]
    pub babeldoc_engine: Option<PathBuf>,
    /// The third PDF output, "for papers (BabelDOC)". It takes effect only while the engine
    /// above checks as usable; otherwise `pdf_layout` decides between the other two, as before.
    pub pdf_babeldoc: bool,
    /// PLaMo's target language. An external model never changes it.
    pub target_language: TargetChoice,
    pub translation_style: StyleChoice,
    /// The pair last chosen with an external model, which translates fixed pairs only. Kept
    /// apart from PLaMo's target, so that neither kind of model overwrites the other's choice.
    #[serde(deserialize_with = "lenient")]
    pub external_languages: Option<LanguagePair>,
}

/// A source and a target language code.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct LanguagePair {
    pub source: String,
    pub target: String,
}

/// A value of another shape (written by a newer version, or edited by hand) reads as none
/// instead of making the whole file invalid.
fn lenient<'de, D, T>(deserializer: D) -> Result<Option<T>, D::Error>
where
    D: serde::Deserializer<'de>,
    T: serde::de::DeserializeOwned,
{
    let value = serde_json::Value::deserialize(deserializer)?;
    Ok(serde_json::from_value(value).ok())
}

fn locale_parts(locale: &str) -> (&str, Option<&str>) {
    let mut parts = locale.split(['-', '_', '.', '@']);
    (parts.next().unwrap_or_default(), parts.next())
}

fn chinese_is_traditional(subtag: Option<&str>) -> bool {
    subtag.is_some_and(|subtag| {
        ["hant", "tw", "hk", "mo"]
            .iter()
            .any(|tag| subtag.eq_ignore_ascii_case(tag))
    })
}

fn locale_target(locale: &str) -> Option<&'static str> {
    let (language, subtag) = locale_parts(locale);
    match language.to_ascii_lowercase().as_str() {
        "zh" if chinese_is_traditional(subtag) => Some("zh-hant"),
        "zh" => Some("zh-hans"),
        "pt" => Some("pt"),
        code => find_language(code).map(|language| language.code),
    }
}

fn preferred_translation_target(
    locales: impl IntoIterator<Item = impl AsRef<str>>,
) -> &'static str {
    locales
        .into_iter()
        .find_map(|locale| locale_target(locale.as_ref()))
        .unwrap_or("en")
}

impl Default for Settings {
    fn default() -> Self {
        Self {
            first_run_setup_pending: true,
            model: None,
            model_fixed: false,
            model_high_quality: false,
            models_dir: None,
            license_accepted: false,
            backend: BackendSettings::default(),
            glossary: String::new(),
            ui_language: UiLanguage::default(),
            theme: ThemeSetting::default(),
            ui_scale: 100,
            document: DocumentSettings::default(),
            sync_scroll: true,
            check_updates: true,
            double_copy: false,
            resident: false,
            pdf_layout: false,
            babeldoc_engine: None,
            pdf_babeldoc: false,
            target_language: TargetChoice::Lang(preferred_translation_target(
                sys_locale::get_locales(),
            )),
            translation_style: StyleChoice::default(),
            external_languages: None,
        }
    }
}

impl Settings {
    /// The folder models are downloaded to and loaded from.
    pub fn models_dir(&self) -> PathBuf {
        self.models_dir.clone().unwrap_or_else(default_models_dir)
    }

    /// [`Self::ui_scale`] as a factor, kept within 50–300% whatever settings.json says.
    pub fn ui_scale_factor(&self) -> f32 {
        f32::from(self.ui_scale.clamp(50, 300)) / 100.0
    }
}

/// Target language choice.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize)]
#[serde(into = "Option<&'static str>")]
pub enum TargetChoice {
    /// Japanese text goes to English and everything else to Japanese.
    #[default]
    JapaneseEnglish,
    Lang(&'static str),
}

impl From<TargetChoice> for Option<&'static str> {
    fn from(choice: TargetChoice) -> Self {
        match choice {
            TargetChoice::JapaneseEnglish => None,
            TargetChoice::Lang(code) => Some(code),
        }
    }
}

impl<'de> Deserialize<'de> for TargetChoice {
    fn deserialize<D: serde::Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        let value = serde_json::Value::deserialize(deserializer)?;
        Ok(match value.as_str() {
            Some(JA_EASY) => Self::Lang(JA_EASY),
            Some("pt") => Self::Lang("pt"),
            Some(code) => find_language(code).map_or_else(
                || Settings::default().target_language,
                |lang| Self::Lang(lang.code),
            ),
            None if value.is_null() => Self::JapaneseEnglish,
            None => Settings::default().target_language,
        })
    }
}

impl TargetChoice {
    /// The concrete target language for a source text, given whether it [`crate::looks_japanese`].
    pub fn resolve_for(self, source_is_japanese: bool) -> &'static str {
        match self {
            Self::Lang(code) => code,
            Self::JapaneseEnglish if source_is_japanese => "en",
            Self::JapaneseEnglish => "ja",
        }
    }
}

/// Japanese output style.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "snake_case", from = "serde_json::Value")]
pub enum StyleChoice {
    /// Leave the style to the model.
    #[default]
    Default,
    Polite,
    Plain,
}

impl From<serde_json::Value> for StyleChoice {
    fn from(value: serde_json::Value) -> Self {
        match value.as_str() {
            Some("polite") => Self::Polite,
            Some("plain") => Self::Plain,
            _ => Self::Default,
        }
    }
}

impl StyleChoice {
    pub fn form(self) -> Option<Form> {
        match self {
            Self::Default => None,
            Self::Polite => Some(Form::Polite),
            Self::Plain => Some(Form::Plain),
        }
    }
}

/// How long texts are split (see `engine::document`).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct DocumentSettings {
    /// Upper bound for one chunk, in tokens.
    pub chunk_tokens: usize,
    /// Upper bound for the history sent with each chunk, in tokens.
    pub history_tokens: usize,
}

impl Default for DocumentSettings {
    fn default() -> Self {
        let defaults = DocumentOptions::default();
        Self {
            chunk_tokens: defaults.max_chunk_tokens,
            history_tokens: defaults.max_history_tokens,
        }
    }
}

impl DocumentSettings {
    /// The options for a translation; `cut_marker` marks passages that were cut.
    pub fn options(self, cut_marker: String) -> DocumentOptions {
        DocumentOptions {
            // A chunk of a few tokens would only split sentences apart.
            max_chunk_tokens: self.chunk_tokens.max(64),
            max_history_tokens: self.history_tokens,
            cut_marker,
        }
    }
}

/// Light or dark appearance.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ThemeSetting {
    /// Follow the operating system, including later changes.
    #[default]
    System,
    Light,
    Dark,
}

/// Language of the user interface.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(into = "&'static str", from = "serde_json::Value")]
pub enum UiLanguage {
    /// Follow the operating system.
    #[default]
    System,
    English,
    Japanese,
    ChineseSimplified,
    ChineseTraditional,
    Korean,
    Spanish,
    French,
    German,
    PortugueseBrazil,
    Indonesian,
}

impl UiLanguage {
    /// Explicit choices, with their locale tags and endonyms in menu order.
    pub const OPTIONS: &[(Self, &str, &str)] = &[
        (Self::English, "en", "English"),
        (Self::Japanese, "ja", "日本語"),
        (Self::ChineseSimplified, "zh-Hans", "简体中文"),
        (Self::ChineseTraditional, "zh-Hant", "繁體中文"),
        (Self::Korean, "ko", "한국어"),
        (Self::Spanish, "es", "Español"),
        (Self::French, "fr", "Français"),
        (Self::German, "de", "Deutsch"),
        (Self::PortugueseBrazil, "pt-BR", "Português (Brasil)"),
        (Self::Indonesian, "id", "Bahasa Indonesia"),
    ];

    fn explicit_locale(self) -> Option<&'static str> {
        Self::OPTIONS
            .iter()
            .find_map(|&(language, tag, _)| (language == self).then_some(tag))
    }

    /// The locale code to use. `PRIVATE_TRANSLATE_LANG` overrides everything, for development.
    pub fn locale(self) -> &'static str {
        std::env::var("PRIVATE_TRANSLATE_LANG")
            .ok()
            .as_deref()
            .and_then(supported_locale)
            .or_else(|| self.explicit_locale())
            .unwrap_or_else(system_locale)
    }
}

impl From<UiLanguage> for &'static str {
    fn from(language: UiLanguage) -> Self {
        match language {
            UiLanguage::English => "english",
            UiLanguage::Japanese => "japanese",
            _ => language.explicit_locale().unwrap_or("system"),
        }
    }
}

impl From<serde_json::Value> for UiLanguage {
    fn from(value: serde_json::Value) -> Self {
        match value.as_str() {
            Some("english") => Self::English,
            Some("japanese") => Self::Japanese,
            Some(tag) => Self::OPTIONS
                .iter()
                .find_map(|&(language, code, _)| (code == tag).then_some(language))
                .unwrap_or_default(),
            None => Self::System,
        }
    }
}

/// The first supported OS preference. sys-locale reads Core Foundation's preferred languages on
/// macOS and GetUserPreferredUILanguages on Windows.
pub fn system_locale() -> &'static str {
    preferred_ui_locale(sys_locale::get_locales())
}

fn preferred_ui_locale(locales: impl IntoIterator<Item = impl AsRef<str>>) -> &'static str {
    locales
        .into_iter()
        .find_map(|locale| supported_locale(locale.as_ref()))
        .unwrap_or("en")
}

fn supported_locale(requested: &str) -> Option<&'static str> {
    let (language, subtag) = locale_parts(requested);
    match language.to_ascii_lowercase().as_str() {
        "zh" if chinese_is_traditional(subtag) => Some("zh-Hant"),
        "zh" => Some("zh-Hans"),
        "yue" if subtag.is_some_and(|tag| tag.eq_ignore_ascii_case("Hans")) => Some("zh-Hans"),
        "yue" => Some("zh-Hant"),
        "pt" => Some("pt-BR"),
        code => UiLanguage::OPTIONS
            .iter()
            .find_map(|&(_, tag, _)| (tag == code).then_some(tag)),
    }
}

/// Where and how the model runs.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct BackendSettings {
    pub device: DeviceSetting,
    /// The fastest device measured on this machine, which [`DeviceSetting::Auto`] uses while
    /// the devices are the same (see `crate::devices`).
    pub measured: Option<MeasuredChoice>,
}

impl Default for BackendSettings {
    fn default() -> Self {
        Self {
            device: DeviceSetting::Auto,
            measured: None,
        }
    }
}

/// The result of measuring the devices.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct MeasuredChoice {
    /// The devices measured, which the choice holds for (see `crate::devices::hardware`).
    pub hardware: String,
    /// The fastest: a device name such as `Vulkan0`, or `None` for the CPU.
    pub device: Option<String>,
    /// Each device's speed, for showing.
    pub results: Vec<MeasuredDevice>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct MeasuredDevice {
    /// A device name, or `CPU`.
    pub name: String,
    /// Generation speed in tokens per second, or `None` if the model did not run there.
    pub tokens_per_second: Option<f64>,
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case", tag = "kind", content = "name")]
pub enum DeviceSetting {
    /// The measured fastest device (see [`BackendSettings::measured`]), else the discrete GPU
    /// with the most free memory, else the CPU.
    #[default]
    Auto,
    Cpu,
    /// A ggml device by name, such as `Vulkan0`.
    Device(String),
}

impl Settings {
    /// Loads the settings, falling back to defaults if the file is missing or unreadable.
    ///
    /// A file that does not parse is renamed to `settings.json.invalid` rather than overwritten
    /// by the next save, so the license acceptance and the glossary in it can be recovered.
    pub fn load() -> Self {
        Self::read_from_disk(true)
    }

    /// Reads settings without moving a malformed file, which may come from a newer version.
    pub fn read() -> Self {
        Self::read_from_disk(false)
    }

    fn read_from_disk(move_invalid: bool) -> Self {
        match settings_path() {
            Some(path) => Self::read_path(&path, move_invalid),
            None => Self::default(),
        }
    }

    fn read_path(path: &std::path::Path, move_invalid: bool) -> Self {
        let bytes = match std::fs::read(path) {
            Ok(bytes) => bytes,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Self::default(),
            Err(e) => {
                crate::app_log!("cannot read the settings in {}: {e}", path.display());
                return Self::default();
            }
        };
        // Editors on Windows may save it with a byte order mark, or in another encoding; the
        // latter is kept aside like any file that does not parse, rather than overwritten.
        let bytes = bytes.strip_prefix(b"\xEF\xBB\xBF").unwrap_or(&bytes);
        let parsed = std::str::from_utf8(bytes)
            .map_err(|e| format!("not UTF-8: {e}"))
            .and_then(|text| serde_json::from_str(text).map_err(|e| e.to_string()));
        parsed.unwrap_or_else(|e| {
            let aside = path.with_extension("json.invalid");
            let moved = move_invalid && std::fs::rename(path, &aside).is_ok();
            crate::app_log!(
                "ignoring invalid settings in {}: {e}{}",
                path.display(),
                if moved {
                    format!("; kept as {}", aside.display())
                } else {
                    String::new()
                }
            );
            Self::default()
        })
    }

    /// Saves the settings atomically. Failures are reported but not fatal.
    pub fn save(&self) {
        let Some(path) = settings_path() else {
            return;
        };
        let result = (|| -> std::io::Result<()> {
            if let Some(dir) = path.parent() {
                std::fs::create_dir_all(dir)?;
            }
            let tmp = path.with_extension("json.tmp");
            std::fs::write(&tmp, serde_json::to_string_pretty(self)?)?;
            std::fs::rename(&tmp, &path)
        })();
        if let Err(e) = result {
            crate::app_log!("failed to save settings to {}: {e}", path.display());
        }
    }
}

#[cfg(test)]
mod tests {
    use super::Settings;

    #[test]
    fn imports_trial_settings_and_reuses_models_without_changing_the_source() {
        let temp = tempfile::tempdir().unwrap();
        let old = temp.path().join("trial");
        let old_models = old.join("models");
        let source = old.join("settings.json");
        let new = temp.path().join("release");
        let destination = new.join("settings.json");
        let original = br#"{"license_accepted":true,"unknown_option":42}"#;
        std::fs::create_dir_all(&old_models).unwrap();
        std::fs::write(old_models.join("model.gguf"), b"fake model").unwrap();
        std::fs::write(&source, original).unwrap();
        let sources = [(source.clone(), old_models.clone())];

        assert!(super::import_settings(&destination, &new.join("models"), &sources).unwrap());
        let imported = std::fs::read(&destination).unwrap();
        let json: serde_json::Value = serde_json::from_slice(&imported).unwrap();
        assert_eq!(json["unknown_option"], 42);
        assert!(Settings::read_path(&destination, false).license_accepted);
        assert_eq!(
            Settings::read_path(&destination, false).models_dir,
            Some(old_models.clone())
        );
        assert_eq!(std::fs::read(&source).unwrap(), original);
        assert_eq!(
            std::fs::read(old_models.join("model.gguf")).unwrap(),
            b"fake model"
        );
        assert!(!new.join("models").exists());

        std::fs::write(&source, b"{}").unwrap();
        assert!(!super::import_settings(&destination, &new.join("models"), &sources).unwrap());
        assert_eq!(std::fs::read(&destination).unwrap(), imported);
    }

    #[test]
    fn imported_trial_settings_use_the_default_update_preference() {
        let temp = tempfile::tempdir().unwrap();
        let source = temp.path().join("trial.json");
        let destination = temp.path().join("release.json");
        let models = temp.path().join("models");
        let original = br#"{"check_updates":false,"license_accepted":true,"unknown_option":42}"#;
        std::fs::write(&source, original).unwrap();
        let sources = [(source.clone(), temp.path().join("trial-models"))];

        assert!(super::import_settings(&destination, &models, &sources).unwrap());
        let imported = Settings::read_path(&destination, false);
        assert!(imported.check_updates);
        assert!(imported.license_accepted);
        let json: serde_json::Value =
            serde_json::from_slice(&std::fs::read(&destination).unwrap()).unwrap();
        assert!(json.get("check_updates").is_none());
        assert_eq!(json["unknown_option"], 42);
        assert_eq!(std::fs::read(&source).unwrap(), original);
    }

    #[test]
    fn preserves_a_custom_model_directory_and_respects_existing_new_models() {
        let temp = tempfile::tempdir().unwrap();
        let root = temp.path();
        let old = root.join("old.json");
        let old_models = root.join("old-models");
        let models = root.join("models");
        std::fs::create_dir(&old_models).unwrap();
        std::fs::create_dir(&models).unwrap();
        let sources = [(old.clone(), old_models)];

        std::fs::write(&old, b"{}").unwrap();
        let destination = root.join("new.json");
        assert!(super::import_settings(&destination, &models, &sources).unwrap());
        assert_eq!(std::fs::read(&destination).unwrap(), b"{}");

        let custom = root.join("custom");
        std::fs::write(
            &old,
            serde_json::to_vec(&serde_json::json!({"models_dir": custom})).unwrap(),
        )
        .unwrap();
        let destination = root.join("custom.json");
        assert!(super::import_settings(&destination, &root.join("absent"), &sources).unwrap());
        assert_eq!(
            Settings::read_path(&destination, false).models_dir,
            Some(custom)
        );
    }

    #[test]
    fn reads_a_byte_order_mark_and_keeps_other_encodings_aside() {
        let dir = std::env::temp_dir().join(format!("plamo-settings-test-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("settings.json");
        std::fs::write(&path, b"\xEF\xBB\xBF{\"license_accepted\": true}").unwrap();
        assert!(Settings::read_path(&path, true).license_accepted);
        // Not UTF-8 (Shift_JIS): kept aside rather than read as garbage and saved over.
        std::fs::write(&path, b"{\"glossary\": \"\x93\xfa\x96\x7b\"}").unwrap();
        assert!(Settings::read_path(&path, true).glossary.is_empty());
        assert!(!path.exists());
        assert!(path.with_extension("json.invalid").exists());
        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn missing_fields_take_defaults() {
        let s: Settings = serde_json::from_str("{}").unwrap();
        assert_eq!(
            s,
            Settings {
                first_run_setup_pending: false,
                ..Settings::default()
            }
        );
        assert_eq!(s.ui_scale_factor(), 1.0);
        let s: Settings = serde_json::from_str(r#"{"ui_scale":150}"#).unwrap();
        assert_eq!(s.ui_scale_factor(), 1.5);
        let s: Settings = serde_json::from_str(r#"{"ui_scale":1000}"#).unwrap();
        assert_eq!(s.ui_scale_factor(), 3.0);
        assert_eq!(s.models_dir(), super::default_models_dir());
        let s: Settings = serde_json::from_str(r#"{"models_dir":"/srv/models"}"#).unwrap();
        assert_eq!(s.models_dir(), std::path::Path::new("/srv/models"));
        let s: Settings = serde_json::from_str(r#"{"model":"x","unknown":1}"#).unwrap();
        assert_eq!(s.model.as_deref(), Some("x"));
        assert!(!s.license_accepted);
        // Settings from older versions with a context length still load.
        let s: Settings =
            serde_json::from_str(r#"{"backend":{"device":{"kind":"cpu"},"n_ctx":4096}}"#).unwrap();
        assert_eq!(s.backend.device, super::DeviceSetting::Cpu);
        assert_eq!(s.ui_language, super::UiLanguage::System);
        let s: Settings = serde_json::from_str(r#"{"ui_language":"japanese"}"#).unwrap();
        assert_eq!(s.ui_language, super::UiLanguage::Japanese);
        assert_eq!(s.theme, super::ThemeSetting::System);
        let s: Settings = serde_json::from_str(r#"{"theme":"dark"}"#).unwrap();
        assert_eq!(s.theme, super::ThemeSetting::Dark);
        assert_eq!(s.document, super::DocumentSettings::default());
        assert!(s.sync_scroll);
        let s: Settings = serde_json::from_str(r#"{"document":{"chunk_tokens":512}}"#).unwrap();
        assert_eq!(s.document.chunk_tokens, 512);
        assert_eq!(
            s.document.history_tokens,
            super::DocumentSettings::default().history_tokens
        );
    }

    #[test]
    fn ui_locale_follows_the_first_supported_os_preference() {
        use super::preferred_ui_locale;

        assert_eq!(preferred_ui_locale(["it", "ko", "en"]), "ko");
        assert_eq!(preferred_ui_locale(["it", "en-GB", "ko"]), "en");
        for tag in ["zh", "zh-Hans", "zh-CN", "zh-SG", "yue-Hans", "yue-Hans-CN"] {
            assert_eq!(preferred_ui_locale([tag]), "zh-Hans", "{tag}");
        }
        for tag in [
            "zh-Hant",
            "zh-TW",
            "zh-HK",
            "zh-MO",
            "zh-Hant-HK",
            "yue",
            "yue-Hant",
            "yue-HK",
        ] {
            assert_eq!(preferred_ui_locale([tag]), "zh-Hant", "{tag}");
        }
        for tag in ["pt", "pt-BR", "pt-PT"] {
            assert_eq!(preferred_ui_locale([tag]), "pt-BR", "{tag}");
        }
        assert_eq!(preferred_ui_locale(["ar"]), "en");
        assert_eq!(preferred_ui_locale([] as [&str; 0]), "en");
    }

    #[test]
    fn ui_language_round_trips_and_unknown_values_keep_other_settings() {
        use super::UiLanguage;

        for language in std::iter::once(UiLanguage::System)
            .chain(UiLanguage::OPTIONS.iter().map(|&(language, _, _)| language))
        {
            let settings = Settings {
                ui_language: language,
                ..Settings::default()
            };
            let value = serde_json::to_value(&settings).unwrap();
            assert_eq!(
                serde_json::from_value::<Settings>(value)
                    .unwrap()
                    .ui_language,
                language
            );
        }
        for value in [r#""future-language""#, "null", "123"] {
            let settings: Settings = serde_json::from_str(&format!(
                r#"{{"ui_language":{value},"license_accepted":true,"target_language":"ja"}}"#
            ))
            .unwrap();
            assert_eq!(settings.ui_language, UiLanguage::System);
            assert_eq!(settings.target_language, super::TargetChoice::Lang("ja"));
            assert!(settings.license_accepted);
        }
    }

    #[test]
    fn default_target_uses_translation_languages_in_os_order() {
        use super::preferred_translation_target;

        for (os, target) in [
            ("zh-CN", "zh-hans"),
            ("zh-HK", "zh-hant"),
            ("ko-KR", "ko"),
            ("es-MX", "es"),
            ("fr-CA", "fr"),
            ("de-AT", "de"),
            ("pt-PT", "pt"),
            ("id-ID", "id"),
            ("ar", "ar"),
        ] {
            assert_eq!(preferred_translation_target([os, "en"]), target, "{os}");
        }
        assert_eq!(preferred_translation_target(["sv", "ko", "en"]), "ko");
        assert_eq!(preferred_translation_target(["it", "ko"]), "it");
        assert_eq!(preferred_translation_target([] as [&str; 0]), "en");
    }

    #[test]
    fn pdf_layout_is_an_opt_in_boolean_and_old_settings_still_load() {
        let old: Settings = serde_json::from_str(r#"{"glossary":"source = target"}"#).unwrap();
        assert!(!old.pdf_layout);
        let selected = Settings {
            pdf_layout: true,
            ..old
        };
        let json = serde_json::to_value(&selected).unwrap();
        assert_eq!(json["pdf_layout"], true);
        assert_eq!(serde_json::from_value::<Settings>(json).unwrap(), selected);
    }

    #[test]
    fn model_preferences_default_and_round_trip() {
        let legacy: Settings = serde_json::from_str(r#"{"model":"hy-mt2"}"#).unwrap();
        assert!(!legacy.model_fixed);
        assert!(!legacy.model_high_quality);
        let selected = Settings {
            model_fixed: true,
            model_high_quality: true,
            ..legacy
        };
        let json = serde_json::to_string(&selected).unwrap();
        assert_eq!(serde_json::from_str::<Settings>(&json).unwrap(), selected);
    }

    #[test]
    fn the_babeldoc_output_is_opt_in_and_old_settings_still_load() {
        // Settings written before the BabelDOC output: no engine, the two old choices.
        let old: Settings =
            serde_json::from_str(r#"{"pdf_layout":true,"glossary":"a = b"}"#).unwrap();
        assert_eq!(old.babeldoc_engine, None);
        assert!(!old.pdf_babeldoc);
        assert!(!old.model_fixed);
        assert!(!old.model_high_quality);
        assert!(old.pdf_layout);
        assert_eq!(Settings::default().babeldoc_engine, None);
        assert!(!Settings::default().pdf_babeldoc);
        // The new values round-trip, and pdf_layout stays a plain boolean beside them.
        let chosen = Settings {
            babeldoc_engine: Some(std::path::PathBuf::from(
                "/opt/babeldoc/bin/babeldoc-engine",
            )),
            pdf_babeldoc: true,
            model_fixed: true,
            model_high_quality: true,
            ..old
        };
        let json = serde_json::to_value(&chosen).unwrap();
        assert_eq!(json["pdf_layout"], true);
        assert_eq!(json["pdf_babeldoc"], true);
        assert_eq!(serde_json::from_value::<Settings>(json).unwrap(), chosen);
        // An engine value of another shape resets only that value.
        let odd: Settings =
            serde_json::from_str(r#"{"babeldoc_engine":42,"pdf_babeldoc":true}"#).unwrap();
        assert_eq!(odd.babeldoc_engine, None);
        assert!(odd.pdf_babeldoc);
    }

    #[test]
    fn translation_choices_round_trip_and_unknown_values_only_reset_the_choice() {
        use super::{StyleChoice, TargetChoice};
        let old: Settings = serde_json::from_str(r#"{"pdf_layout":true}"#).unwrap();
        assert_eq!(old.target_language, Settings::default().target_language);
        assert_eq!(old.translation_style, StyleChoice::Default);
        for target in [
            TargetChoice::Lang("fr"),
            TargetChoice::Lang(super::JA_EASY),
            TargetChoice::JapaneseEnglish,
        ] {
            for style in [
                StyleChoice::Default,
                StyleChoice::Polite,
                StyleChoice::Plain,
            ] {
                let settings = Settings {
                    target_language: target,
                    translation_style: style,
                    ..old.clone()
                };
                let json = serde_json::to_value(&settings).unwrap();
                assert!(json.get("context").is_none());
                assert_eq!(serde_json::from_value::<Settings>(json).unwrap(), settings);
            }
        }
        for unknown in [
            serde_json::json!("future"),
            serde_json::json!({"future": 1}),
        ] {
            let json = serde_json::json!({"pdf_layout":true, "translation_style":unknown, "target_language":unknown});
            assert_eq!(
                serde_json::from_value::<Settings>(json).unwrap(),
                Settings {
                    target_language: Settings::default().target_language,
                    ..old.clone()
                }
            );
        }
    }

    #[test]
    fn the_external_pair_is_kept_apart_and_read_leniently() {
        use super::{LanguagePair, TargetChoice};
        let old: Settings = serde_json::from_str(r#"{"target_language":"en"}"#).unwrap();
        assert_eq!(old.external_languages, None);
        let settings = Settings {
            external_languages: Some(LanguagePair {
                source: "en".into(),
                target: "pt".into(),
            }),
            ..old.clone()
        };
        let json = serde_json::to_value(&settings).unwrap();
        assert_eq!(json["external_languages"]["target"], "pt");
        assert_eq!(json["target_language"], "en");
        assert_eq!(serde_json::from_value::<Settings>(json).unwrap(), settings);
        for unknown in [
            serde_json::json!("en-pt"),
            serde_json::json!({"source": 1}),
            serde_json::Value::Null,
        ] {
            let json = serde_json::json!({"target_language":"en", "external_languages":unknown});
            assert_eq!(serde_json::from_value::<Settings>(json).unwrap(), old);
        }
        let settings: Settings = serde_json::from_str(r#"{"target_language":"pt"}"#).unwrap();
        assert_eq!(settings.target_language, TargetChoice::Lang("pt"));
    }

    #[test]
    fn device_setting_round_trips() {
        use super::DeviceSetting;
        for d in [
            DeviceSetting::Auto,
            DeviceSetting::Cpu,
            DeviceSetting::Device("Vulkan0".into()),
        ] {
            let json = serde_json::to_string(&d).unwrap();
            assert_eq!(serde_json::from_str::<DeviceSetting>(&json).unwrap(), d);
        }
    }
}

#[cfg(test)]
#[test]
fn system_locale_maps_to_translation_languages() {
    for (locale, expected) in [
        ("zh-Hans-CN", Some("zh-hans")),
        ("zh-TW", Some("zh-hant")),
        ("zh-HK", Some("zh-hant")),
        ("pt-BR", Some("pt")),
        ("pt-PT", Some("pt")),
        ("ko-KR", Some("ko")),
        ("es-MX", Some("es")),
        ("ja-JP", Some("ja")),
        ("en-GB", Some("en")),
        ("zz-ZZ", None),
    ] {
        assert_eq!(locale_target(locale), expected);
    }
}
