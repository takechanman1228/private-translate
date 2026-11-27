//! The main translator window.

#[path = "files.rs"]
mod files;

use std::collections::VecDeque;
use std::ops::Range;
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use engine::StopReason;
use engine::llama::{Device, ModelProfile, RuntimeOptions};
use engine::models::{self as catalog, CatalogEntry, LocalState, ModelStore};
use engine::pfml::{GlossaryEntry, LangCode, TranslationOptions};
use engine::profiles::Capabilities;
use engine::selection::{self, QualityTier};
use engine::worker::{Event, JobId, LoadFailure, LoadRequest, MeasureRequest, Worker};
use gpui_kit::assets::IconName as Lucide;
use gpui_kit::component::button::{Button, ButtonVariants as _};
use gpui_kit::component::input::{
    BufferPoint, DisplayPoint, InputEvent, RopeExt as _, Textarea, TextareaState,
};
use gpui_kit::component::searchable_list::SearchableVec;
use gpui_kit::component::select::{Select, SelectEvent, SelectItem as _, SelectState};
use gpui_kit::component::switch::Switch;
use gpui_kit::component::{
    ActiveTheme as _, Disableable as _, IconName, IndexPath, Selectable as _, Sizable as _,
    StyledExt as _, Theme, ThemeMode, TitleBar, WindowExt as _, h_flex, v_flex,
};
use gpui_kit::prelude::FluentBuilder as _;
use gpui_kit::*;
use rust_i18n::t;
use update::{CheckCache, Installation, Release, UpdateError, Version};

pub(crate) const ACTION_ACCENT: u32 = 0x315cdf;

use crate::align::{Alignment, ChunkSpan, Side};
use crate::backend::{self, BackendEvent, BackendPanel};
use crate::glossary;
use crate::lang::{self, Choice, SourceChoice, StyleChoice, TargetChoice};
use crate::models::{ModelsEvent, ModelsPanel};
use crate::settings::{self, DeviceSetting, DocumentSettings, Settings, ThemeSetting, UiLanguage};
use crate::setup;

actions!(translator, [CopyTranslation, TranslateNow, OpenFile]);

const KEY_CONTEXT: &str = "Translator";
/// Delay between the last keystroke and an automatic translation.
const DEBOUNCE: Duration = Duration::from_millis(500);
/// How long the copy button shows its confirmation.
const COPIED_FEEDBACK: Duration = Duration::from_millis(1500);
/// Number of finished translations kept for instant reuse.
const CACHE_SIZE: usize = 64;
/// While text streams in, the output is redrawn at most this often. A GPU produces 100+
/// tokens per second, and redrawing on every token starves input handling.
const STREAM_FRAME: Duration = Duration::from_millis(50);

#[cfg(target_os = "macos")]
const COPY_TRANSLATION_KEY: &str = "cmd-shift-c";
#[cfg(not(target_os = "macos"))]
const COPY_TRANSLATION_KEY: &str = "ctrl-shift-c";

#[cfg(target_os = "macos")]
const TRANSLATE_NOW_KEY: &str = "cmd-enter";
#[cfg(not(target_os = "macos"))]
const TRANSLATE_NOW_KEY: &str = "ctrl-enter";
/// The key context of gpui-kit's text inputs, where it binds its own keys.
const INPUT_CONTEXT: &str = "Input";

/// Byte offset of the start of the first line visible in `state`.
fn top_offset(state: &TextareaState) -> Option<usize> {
    let line_height = state.line_height()?;
    let row = (-state.scroll_offset().y / line_height).max(0.0) as usize;
    let point = state
        .display_map()
        .display_pos_to_buffer_pos(DisplayPoint::new(row, 0));
    let text = state.text();
    Some((text.line_start_offset(point.line) + point.col).min(text.len()))
}

/// The display row (after wrapping) of byte `offset` in `state`.
fn display_row(state: &TextareaState, offset: usize) -> usize {
    let point = state.text().offset_to_point(offset);
    state
        .display_map()
        .buffer_pos_to_display_pos(BufferPoint::new(point.row, point.column))
        .row
}

/// The model to load from `store`: the saved selection if it is there, else the default model,
/// else the first one there.
fn model_to_load(settings: &Settings, store: &ModelStore) -> Option<&'static CatalogEntry> {
    saved_model(settings)
        .filter(|entry| !entry.is_plamo() || settings.license_accepted)
        // A saved, damaged pinned model goes through normal failure reporting and recovery.
        .filter(|entry| {
            let state = store.state(entry);
            state.is_usable() || matches!(state, LocalState::Invalid { .. })
        })
        .or_else(|| {
            settings
                .license_accepted
                .then(|| store.preferred_plamo())
                .flatten()
        })
}

fn rollback_before_switch(
    previous: Option<ModelChoice>,
    ready: bool,
    active: Option<&ModelChoice>,
    next: &ModelChoice,
) -> Option<ModelChoice> {
    if ready {
        active.filter(|current| *current != next).cloned()
    } else {
        previous
    }
}

fn saved_model(settings: &Settings) -> Option<&'static CatalogEntry> {
    settings.model.as_deref().and_then(catalog::find_entry)
}

fn file_capabilities(
    active_model: Option<&ModelChoice>,
    settings: &Settings,
    profile: ModelProfile,
) -> &'static Capabilities {
    let entry = match active_model {
        Some(choice) => choice.entry(),
        None if settings.model_fixed => saved_model(settings),
        None => None,
    };
    entry.map_or_else(|| profile.capabilities(), CatalogEntry::capabilities)
}

fn startup_model(settings: &Settings, store: &ModelStore) -> Option<&'static CatalogEntry> {
    if let Some(entry) =
        saved_model(settings).filter(|entry| entry.is_plamo() && !settings.license_accepted)
    {
        return store.state(entry).is_usable().then_some(entry);
    }
    model_to_load(settings, store).or_else(|| {
        (!settings.model_fixed)
            .then(|| {
                catalog::entries().into_iter().find(|entry| {
                    (!entry.is_plamo() || settings.license_accepted)
                        && store.state(entry).is_usable()
                })
            })
            .flatten()
    })
}

fn token_choices(document: &DocumentSettings) -> (Vec<Choice<usize>>, Vec<Choice<usize>>) {
    let defaults = DocumentSettings::default();
    (
        lang::token_choices(
            lang::CHUNK_TOKEN_PRESETS,
            document.chunk_tokens,
            defaults.chunk_tokens,
        ),
        lang::token_choices(
            lang::HISTORY_TOKEN_PRESETS,
            document.history_tokens,
            defaults.history_tokens,
        ),
    )
}

/// This build's version, the settings' Check now button, and what it found.
fn update_check_row(
    check: UpdateCheck,
    muted: Hsla,
    view: WeakEntity<TranslatorView>,
) -> AnyElement {
    let checking_key = check.checking_key();
    let checking = checking_key.is_some();
    let result = match check {
        UpdateCheck::Idle | UpdateCheck::Checking | UpdateCheck::Requesting => {
            checking_key.map(|key| t!(key))
        }
        UpdateCheck::UpToDate(version) => {
            Some(t!("settings.update_latest", version = version.to_string()))
        }
        UpdateCheck::Found(version) => Some(t!("update.available", version = version.to_string())),
        UpdateCheck::Failed(error) => Some(t!("settings.update_failed", error = error)),
    };
    h_flex()
        .gap_3()
        .items_center()
        .child(
            div().text_sm().child(
                t!("settings.version", version = Version::current().to_string()).into_owned(),
            ),
        )
        .child(
            Button::new("check-now")
                .small()
                .label(t!("settings.check_now").into_owned())
                .loading(checking)
                .disabled(checking)
                .on_click(move |_, window, cx| {
                    let _ = view.update(cx, |this, cx| {
                        this.check_for_update(UpdateCheckTrigger::Manual, cx)
                    });
                    window.refresh();
                }),
        )
        .children(result.map(|text| {
            div()
                .text_xs()
                .text_color(muted)
                .min_w_0()
                .child(text.into_owned())
        }))
        .into_any_element()
}

/// The theme's font sizes at 100%. The theme's font size is also the rem size, so lengths in rems
/// follow the UI size.
const FONT_SIZE: f32 = 16.;
const MONO_FONT_SIZE: f32 = 13.;

/// A length of `px` pixels at 100%, in rems, so it follows the UI size.
pub fn scaled(px: f32) -> Rems {
    rems(px / FONT_SIZE)
}

/// [`scaled`] in pixels, for what takes no rems (dialog widths, scroll offsets).
pub fn scaled_px(px: f32, cx: &App) -> Pixels {
    cx.theme().font_size * (px / FONT_SIZE)
}

/// Applies the theme (light or dark, following the OS for [`ThemeSetting::System`]) and the UI
/// size of `settings`.
pub fn apply_appearance(settings: &Settings, window: &mut Window, cx: &mut App) {
    match settings.theme {
        ThemeSetting::System => Theme::sync_system_appearance(Some(window), cx),
        ThemeSetting::Light => Theme::change(ThemeMode::Light, Some(window), cx),
        ThemeSetting::Dark => Theme::change(ThemeMode::Dark, Some(window), cx),
    }
    // After the theme, which could bring its own sizes.
    let scale = settings.ui_scale_factor();
    Theme::update(cx, |theme| {
        theme.font_size = px(FONT_SIZE * scale);
        theme.mono_font_size = px(MONO_FONT_SIZE * scale);
    });
    window.refresh();
}

/// How the window opens again while the app stays resident.
const RESIDENT_NOTE: &str = if cfg!(windows) {
    "settings.resident_note_windows"
} else if cfg!(target_os = "macos") {
    "settings.resident_note_macos"
} else {
    "settings.resident_note_linux"
};

/// Binds the window's shortcuts. Call after `gpui_kit::init`: the later binding of a key wins.
pub fn bind_keys(cx: &mut App) {
    cx.bind_keys([
        KeyBinding::new(
            if cfg!(target_os = "macos") {
                "cmd-o"
            } else {
                "ctrl-o"
            },
            OpenFile,
            None,
        ),
        KeyBinding::new(COPY_TRANSLATION_KEY, CopyTranslation, Some(KEY_CONTEXT)),
        // In the text inputs' own context, over gpui-kit's binding of the same key: its Enter
        // action puts a line break into a multi-line input before it tells of the key, with
        // Ctrl (Cmd) held too, so the shortcut that translates would also break the line.
        KeyBinding::new(TRANSLATE_NOW_KEY, TranslateNow, Some(INPUT_CONTEXT)),
    ]);
}

type SourceSelect = SelectState<SearchableVec<Choice<SourceChoice>>>;
type TargetSelect = SelectState<SearchableVec<Choice<TargetChoice>>>;
type StyleSelect = SelectState<Vec<Choice<StyleChoice>>>;
type UiLanguageSelect = SelectState<Vec<Choice<UiLanguage>>>;
type ThemeSelect = SelectState<Vec<Choice<ThemeSetting>>>;
type ScaleSelect = SelectState<Vec<Choice<u16>>>;
type TokensSelect = SelectState<Vec<Choice<usize>>>;

/// A finished translation, kept to show it again without translating.
struct Cached {
    request: Request,
    translation: String,
    chunks: Vec<ChunkSpan>,
    cuts: Vec<CutMark>,
}

/// A passage whose translation kept looping and was cut: the source sentence and what was kept
/// of its translation, as byte ranges.
#[derive(Debug, Clone)]
struct CutMark {
    source: Range<usize>,
    output: Range<usize>,
}

/// What synchronized scrolling last saw of each pane (indexed by `Side as usize`).
#[derive(Default)]
struct SyncState {
    scroll_y: [Pixels; 2],
    selection: [Range<usize>; 2],
    /// When a pane was last scrolled to follow the other; its own changes are ignored briefly,
    /// so the two do not chase each other.
    followed_at: [Option<Instant>; 2],
    /// Notes where a pane scrolled to follow the other came to rest (see [`FOLLOW_APPLIED`]).
    applied: [Option<Task<()>>; 2],
}

/// How long a pane's changes are ignored after it was scrolled to follow the other.
const FOLLOW_SETTLE: Duration = Duration::from_millis(150);

/// How long after a pane was scrolled to follow the other its position is noted. The text input
/// applies a position set from outside only when it is next drawn, and tells nobody: the pane's
/// own notification comes before, with the old position. Noted then, the new position would
/// look like the reader's scrolling at the pane's next notification, whenever that comes (some
/// tenths of a second later in the report that led here), and the other pane would follow it
/// back, a line off. Within
/// [`FOLLOW_SETTLE`], so that nothing takes the old position for a change meanwhile.
const FOLLOW_APPLIED: Duration = Duration::from_millis(100);

/// The chunk of a long text being translated: its byte range in the source, and how many
/// tokens had been generated when it started.
#[derive(Debug, Clone, Copy)]
struct ChunkMark {
    start: usize,
    end: usize,
    tokens_before: usize,
}

/// What the window shows about the source text. Computing it scans the whole text, which is
/// too slow to do on every frame for a long document.
#[derive(Debug, Clone, Copy)]
struct SourceInfo {
    chars: usize,
    looks_japanese: bool,
}

/// One translation request: the text and every option that affects its output.
#[derive(Debug, Clone, PartialEq, Eq)]
struct Request {
    options: TranslationOptions,
    document: DocumentSettings,
    text: String,
}

fn shown_detected_source(
    source: SourceChoice,
    detected: Option<&'static str>,
    request_source: Option<&str>,
) -> Option<&'static str> {
    detected.filter(|&code| source == SourceChoice::Detect && request_source == Some(code))
}

struct DetectedSourceTitle(WeakEntity<TranslatorView>);

impl Render for DetectedSourceTitle {
    fn render(&mut self, _: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let title = self
            .0
            .upgrade()
            .and_then(|view| view.read(cx).detected_source_title());
        div()
            .when(title.is_some(), |label| label.text_size(scaled(13.)))
            .child(title.unwrap_or_else(|| t!("source.detect").into_owned()))
    }
}

#[derive(Clone)]
struct RequestSettings {
    /// The loaded model's profile, which decides what a request may carry.
    profile: ModelProfile,
    source: SourceChoice,
    target: TargetChoice,
    style: StyleChoice,
    document: DocumentSettings,
    context: String,
    glossary: Vec<GlossaryEntry>,
}

impl RequestSettings {
    fn request(&self, text: String, source_is_japanese: bool) -> Request {
        let target = self.target.resolve_for(source_is_japanese);
        let mut options = TranslationOptions::new(LangCode::new(target).expect("built-in code"));
        options.source = self
            .source
            .code()
            .map(|c| LangCode::new(c).expect("built-in code"));
        // External models take no Japanese style; the saved style stays for PLaMo.
        if self.profile.capabilities().japanese_style && options.target.is_japanese() {
            options.form = self.style.form();
        }
        options.context = self.context.clone();
        options.glossary = self.glossary.clone();
        Request {
            options,
            document: self.document,
            text,
        }
    }
}

/// Whether the model and its device stay as they are now. As on main, only a file job (one file
/// or a batch) keeps them: a load, a measurement or a text translation gives way to a newer
/// choice, which the worker takes after it.
fn keeps_model(jobs: &crate::jobs::Jobs) -> bool {
    jobs.busy()
}

/// Whether `choice` loads the file that `entry` installs: the same file in the store, whatever
/// its revision (a newer PLaMo revision is another catalog entry with the same file name).
fn holds_file(choice: &ModelChoice, entry: &CatalogEntry) -> bool {
    choice
        .entry()
        .is_some_and(|loaded| loaded.file_name == entry.file_name)
}

/// How the view installs a verified download based on the worker's loaded model.
#[derive(Debug, PartialEq, Eq)]
enum ReplaceStep {
    /// A file job runs, which keeps the model as it is: decide again when it ends.
    Wait,
    /// The worker holds the file (the active model, loaded or loading): unload it, install the
    /// file once the worker confirms, and load the model again.
    Reload,
    /// The worker does not hold the file: install it now.
    Install,
}

impl ReplaceStep {
    fn of(file_job: bool, active: Option<&ModelChoice>, entry: &'static CatalogEntry) -> Self {
        if file_job {
            Self::Wait
        } else if active.is_some_and(|choice| holds_file(choice, entry)) {
            Self::Reload
        } else {
            Self::Install
        }
    }
}

/// Updates waiting until a file can be replaced. Each unload request sent to the worker gets one
/// list, in the order sent; the worker answers each with one `Event::Unloaded`. An update waits
/// for the request sent when it arrived, so a model loaded after that request is never replaced
/// under the worker.
#[derive(Default)]
struct Releases {
    requests: VecDeque<Vec<&'static CatalogEntry>>,
    /// Updates that arrived during a file job, decided again when it ends.
    deferred: Vec<&'static CatalogEntry>,
}

impl Releases {
    /// An unload request was sent to the worker.
    fn requested(&mut self) {
        self.requests.push_back(Vec::new());
    }

    /// `entry` waits for the unload request sent last.
    fn wait_for_last(&mut self, entry: &'static CatalogEntry) {
        if let Some(waiting) = self.requests.back_mut() {
            push_latest(waiting, entry);
        }
    }

    fn defer(&mut self, entry: &'static CatalogEntry) {
        push_latest(&mut self.deferred, entry);
    }

    fn take_deferred(&mut self) -> Vec<&'static CatalogEntry> {
        std::mem::take(&mut self.deferred)
    }

    /// The worker answered the oldest unload request. Returns the files to install now, and the
    /// model to load afterwards: the updated one, unless another was chosen meanwhile. One chosen
    /// again is loaded again, as its load may have opened the older file.
    fn released(
        &mut self,
        active: Option<&ModelChoice>,
    ) -> (Vec<&'static CatalogEntry>, Option<&'static CatalogEntry>) {
        let installs = self.requests.pop_front().unwrap_or_default();
        let reload = match active {
            None => installs.last().copied(),
            Some(choice) => installs
                .iter()
                .copied()
                .find(|entry| holds_file(choice, entry)),
        };
        (installs, reload)
    }
}

/// Adds `entry` to `list`, in place of an earlier update of the same file.
fn push_latest(list: &mut Vec<&'static CatalogEntry>, entry: &'static CatalogEntry) {
    list.retain(|waiting| waiting.file_name != entry.file_name);
    list.push(entry);
}

/// The model to run.
#[derive(Debug, Clone, PartialEq)]
pub enum ModelChoice {
    Catalog(&'static CatalogEntry),
    /// A GGUF file given on the command line.
    Custom(PathBuf),
}

impl ModelChoice {
    fn entry(&self) -> Option<&'static CatalogEntry> {
        match self {
            Self::Catalog(entry) => Some(entry),
            Self::Custom(_) => None,
        }
    }

    fn profile(&self) -> ModelProfile {
        self.entry()
            .map_or(ModelProfile::default(), |entry| entry.profile)
    }

    fn name(&self) -> String {
        self.entry().map_or_else(
            || self.label(),
            |entry| {
                entry
                    .name
                    .map_or_else(|| format!("PLaMo {}", entry.size_label()), str::to_owned)
            },
        )
    }

    fn label(&self) -> String {
        match self {
            Self::Catalog(entry) => entry.to_string(),
            Self::Custom(path) => model_label(path),
        }
    }
}

struct LoadRecovery {
    model: ModelChoice,
    error: String,
    kind: LoadFailure,
}

impl LoadRecovery {
    fn message(&self) -> String {
        match self.kind {
            LoadFailure::InvalidFile => t!("models.invalid_file").into_owned(),
            LoadFailure::OutOfMemory => t!("models.load_memory").into_owned(),
            LoadFailure::Other => self.error.clone(),
        }
    }
}

enum Status {
    NoModel,
    Unloaded,
    LicenseRequired,
    Loading(f32),
    /// Measuring device `step` of `count` before a load: on the first start (`first`), or
    /// because the settings asked.
    Measuring {
        first: bool,
        step: usize,
        count: usize,
    },
    Ready,
    Translating {
        target: &'static str,
        /// (current, total) when a long text was split.
        chunk: Option<(usize, usize)>,
    },
    Done {
        /// Generated tokens, including output that was withdrawn and translated again.
        tokens: usize,
        /// From the request to the end of the translation.
        elapsed: Duration,
        tokens_per_second: f64,
        /// Passages whose output kept looping and was cut.
        cut: usize,
    },
    Cached {
        /// Passages of the cached translation that were cut.
        cut: usize,
    },
    Stopped,
    Truncated,
    Error(String),
}

fn runtime_failed_during_load(status: &Status, pending_loads: usize, has_model: bool) -> bool {
    has_model
        && (pending_loads > 0 || matches!(status, Status::Loading(_) | Status::Measuring { .. }))
}

fn inference_worker(events: &async_channel::Sender<Event>) -> Worker {
    let events = events.clone();
    Worker::spawn(RuntimeOptions::default(), move |event| {
        let _ = events.send_blocking(event);
    })
}

/// The settings' Check now, while its result shows there.
#[derive(Debug, Clone, PartialEq)]
enum UpdateCheck {
    Idle,
    Checking,
    Requesting,
    /// No newer release; the newest one's version.
    UpToDate(Version),
    Found(Version),
    Failed(String),
}

impl UpdateCheck {
    fn checking_key(&self) -> Option<&'static str> {
        match self {
            Self::Checking => Some("settings.update_checking"),
            Self::Requesting => Some("settings.update_checking_github"),
            _ => None,
        }
    }
}

#[derive(Clone, Copy)]
enum UpdateCheckTrigger {
    Startup,
    Automatic,
    Manual,
}

impl UpdateCheckTrigger {
    fn probes_installation(self) -> bool {
        matches!(self, Self::Startup)
    }

    fn can_check_network(self, automatic_enabled: bool) -> bool {
        automatic_enabled || matches!(self, Self::Manual)
    }
}

/// Installing a newer release from the app.
#[derive(Debug, Clone, PartialEq)]
enum InstallState {
    Idle,
    Downloading {
        version: Version,
        done: u64,
        total: u64,
    },
    /// Unpacked next to the app: a restart swaps it in.
    Ready(Version),
    Verifying(Version),
    Failed(String),
}

pub struct TranslatorView {
    files: files::FilePanel,
    batch: files::BatchPanel,
    /// The check of the external BabelDOC engine (the third PDF output).
    external_engine: files::ExternalEngine,
    /// A close (`false`) or quit (`true`) waiting for a stopped BabelDOC run to clean up.
    pending_close: files::PendingClose,
    picking_files: bool,
    file_tab: bool,
    jobs: crate::jobs::Jobs,
    source: Entity<TextareaState>,
    output: Entity<TextareaState>,
    source_select: Entity<SourceSelect>,
    target_select: Entity<TargetSelect>,
    style_select: Entity<StyleSelect>,
    ui_language_select: Entity<UiLanguageSelect>,
    theme_select: Entity<ThemeSelect>,
    ui_scale_select: Entity<ScaleSelect>,
    chunk_select: Entity<TokensSelect>,
    history_select: Entity<TokensSelect>,
    /// The source and target offered for the loaded model, and its profile.
    languages: lang::Languages,
    auto_translate: bool,
    context_input: Entity<TextareaState>,
    glossary_input: Entity<TextareaState>,
    show_options: bool,
    context: String,
    glossary: Vec<GlossaryEntry>,

    worker: Worker,
    worker_events: async_channel::Sender<Event>,
    worker_failed: bool,
    models: Entity<ModelsPanel>,
    models_folder_was_missing: bool,
    backend: Entity<BackendPanel>,
    /// Compute devices, once llama.cpp has started.
    devices: Option<Vec<Device>>,
    /// Device label of the loaded (or loading) model.
    loaded_on: Option<String>,
    /// A model to delete once the worker confirms it released the file.
    pending_delete: Option<&'static CatalogEntry>,
    /// Verified updates waiting for the worker to release a file (see [`ModelsEvent::Verified`]).
    releases: Releases,
    settings: Settings,
    active_model: Option<ModelChoice>,
    /// The last working model to reload if a requested switch fails.
    rollback_model: Option<ModelChoice>,
    failed_switch: Option<String>,
    model_notice: Option<String>,
    model_menu: bool,
    file_on_load: Option<Request>,
    load_for_file: bool,
    deferred_model_use: Option<&'static CatalogEntry>,
    batch_on_load: Option<usize>,
    required_model: Option<&'static CatalogEntry>,
    required_format: Option<engine::profiles::FileFormat>,
    /// The model currently being prepared by the setup card, or its last failed attempt.
    setup_attempt: Option<&'static CatalogEntry>,
    load_recovery: Option<LoadRecovery>,
    setup_memory: u64,
    model_ready: bool,
    status: Status,
    /// The job whose output is streaming into the output pane.
    current_job: Option<JobId>,
    /// The request whose result is shown (or being produced) in the output pane.
    shown: Option<Request>,
    translation: String,
    /// The model that produced `translation`.
    translated_with: Option<ModelChoice>,
    /// Length of `translation` in UTF-16 code units, the unit of text input ranges.
    translation_utf16: usize,
    /// Derived from the source text when it last changed, instead of on every frame.
    source_info: Option<SourceInfo>,
    cache: VecDeque<Cached>,
    /// Chunks of the shown translation, for synchronized scrolling.
    chunks: Vec<ChunkSpan>,
    /// Passages of the shown translation that kept looping and were cut, and the one the status
    /// bar shows next.
    cuts: Vec<CutMark>,
    next_cut: usize,
    /// The alignment of the shown translation, keyed by its length, its chunk count, and whether
    /// it is complete.
    alignment: Option<((usize, usize, bool), Alignment)>,
    /// Incremented when the source text changes; the alignment holds only while it matches
    /// `shown_version`.
    source_version: u64,
    shown_version: u64,
    sync: SyncState,
    /// Tokens received for the current job, and when the first one arrived (for the live rate).
    stream_tokens: usize,
    stream_started: Option<Instant>,
    /// When the current job was submitted.
    job_started: Option<Instant>,
    /// The chunk being translated, for the remaining-time estimate.
    current_chunk: Option<ChunkMark>,

    /// Set by the paste hook so the following change translates without debouncing.
    paste_pending: bool,
    detected_text_source: Option<lang::DetectedTextSource>,
    /// A translation was asked for while the model was loading: it runs once the model is
    /// ready, also with Auto off (Auto on translates then anyway).
    translate_on_load: bool,
    /// Loads sent to the worker whose result has not arrived. Each load ends in exactly one
    /// `Loaded` or `LoadFailed`; only the last one's events describe the model in use.
    pending_loads: usize,
    /// The last fatal inference error, kept until a model loads successfully.
    runtime_error: Option<String>,
    /// A device measurement is running: `Some(true)` for the first start's. Loads wait for it.
    measuring: Option<bool>,
    /// The settings asked to measure the devices with the next load.
    measure_requested: bool,
    /// The shown translation was stopped or failed part-way, so it neither satisfies a repeated
    /// request nor goes into the cache.
    shown_partial: bool,
    /// A release newer than this build, shown in the status bar.
    newer_release: Option<Release>,
    update_check: UpdateCheck,
    _update_task: Task<()>,
    /// This copy's package, if it came from one: it can then install a newer release itself.
    installation: Option<Result<Installation, UpdateError>>,
    install: InstallState,
    _install_task: Task<()>,
    copied: bool,
    debounce: Task<()>,
    copied_reset: Task<()>,
    _subscriptions: Vec<Subscription>,
    _events: Task<()>,
}

const RELEASE_UPDATE_CHECK_ENABLED: bool = true;

fn if_updates_enabled(enabled: bool, check: impl FnOnce()) {
    if enabled {
        check();
    }
}

impl TranslatorView {
    /// `custom_model` (from the command line) takes precedence over the saved selection.
    pub fn new(
        custom_model: Option<PathBuf>,
        settings: Settings,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Self {
        // Development aid (debug builds only): start with the contents of a file as the source.
        let initial_source = std::env::var_os("PRIVATE_TRANSLATE_DEBUG_SOURCE")
            .filter(|_| cfg!(debug_assertions))
            .and_then(|path| std::fs::read_to_string(path).ok())
            .unwrap_or_default();
        let source = cx.new(|cx| {
            TextareaState::new(window, cx)
                .placeholder(t!("source.placeholder").into_owned())
                .default_value(initial_source)
        });
        let output = cx.new(|cx| TextareaState::new(window, cx));
        let context_input = cx.new(|cx| {
            TextareaState::new(window, cx)
                .placeholder(t!("options.context_placeholder").into_owned())
        });
        let glossary_text = settings.glossary.clone();
        let glossary_text_for_parse = glossary_text.clone();
        let glossary_input = cx.new(|cx| {
            TextareaState::new(window, cx)
                .placeholder(t!("options.glossary_placeholder").into_owned())
                .default_value(glossary_text)
        });

        let source_choice = SourceChoice::Detect;
        let target_choice = settings.target_language;
        let style_choice = settings.translation_style;
        let source_select = {
            let choices = lang::source_choices();
            let selected = lang::position(&choices, &source_choice).map(IndexPath::new);
            cx.new(|cx| {
                SelectState::new(SearchableVec::new(choices), selected, window, cx).searchable(true)
            })
        };
        let target_select = {
            let choices = lang::target_choices();
            let selected = lang::position(&choices, &target_choice).map(IndexPath::new);
            cx.new(|cx| {
                SelectState::new(SearchableVec::new(choices), selected, window, cx).searchable(true)
            })
        };
        let style_select = {
            let choices = lang::style_choices();
            let selected = lang::position(&choices, &style_choice).map(IndexPath::new);
            cx.new(|cx| SelectState::new(choices, selected, window, cx))
        };
        let ui_language_select = {
            let choices = lang::ui_language_choices();
            let selected = lang::position(&choices, &settings.ui_language).map(IndexPath::new);
            cx.new(|cx| SelectState::new(choices, selected, window, cx))
        };
        let theme_select = {
            let choices = lang::theme_choices();
            let selected = lang::position(&choices, &settings.theme).map(IndexPath::new);
            cx.new(|cx| SelectState::new(choices, selected, window, cx))
        };
        let ui_scale_select = {
            let choices = lang::ui_scale_choices(settings.ui_scale);
            let selected = lang::position(&choices, &settings.ui_scale).map(IndexPath::new);
            cx.new(|cx| SelectState::new(choices, selected, window, cx))
        };
        let (chunk_choices, history_choices) = token_choices(&settings.document);
        let chunk_select = {
            let selected = lang::position(&chunk_choices, &settings.document.chunk_tokens);
            cx.new(|cx| SelectState::new(chunk_choices, selected.map(IndexPath::new), window, cx))
        };
        let history_select = {
            let selected = lang::position(&history_choices, &settings.document.history_tokens);
            cx.new(|cx| SelectState::new(history_choices, selected.map(IndexPath::new), window, cx))
        };

        let backend = cx.new(|cx| BackendPanel::new(settings.backend.clone(), window, cx));
        let store = ModelStore::new(settings.models_dir());
        let models = cx.new(|cx| {
            ModelsPanel::new(
                store.clone(),
                None,
                settings.license_accepted,
                settings::catalog_cache_path(),
                cx,
            )
        });

        let subscriptions = vec![
            cx.subscribe_in(&source, window, Self::on_source_event),
            // Scrolling and caret moves notify the pane; the other pane may follow.
            cx.observe_in(&source, window, |this, _, window, cx| {
                this.on_pane_changed(Side::Source, window, cx)
            }),
            cx.observe_in(&output, window, |this, _, window, cx| {
                this.on_pane_changed(Side::Output, window, cx)
            }),
            cx.subscribe_in(&context_input, window, |this, input, event, window, cx| {
                if let InputEvent::Change = event {
                    this.context = input.read(cx).value().to_string();
                    this.options_edited(window, cx);
                }
            }),
            cx.subscribe_in(&glossary_input, window, |this, input, event, window, cx| {
                if let InputEvent::Change = event {
                    let text = input.read(cx).value().to_string();
                    this.glossary = glossary::parse(&text);
                    this.settings.glossary = text;
                    this.settings.save();
                    this.options_edited(window, cx);
                }
            }),
            cx.subscribe_in(&models, window, Self::on_models_event),
            cx.observe(&models, |_, _, cx| cx.notify()),
            cx.subscribe_in(&backend, window, Self::on_backend_event),
            cx.subscribe_in(&source_select, window, |this, _, event, window, cx| {
                if let SelectEvent::Confirm(Some(choice)) = event {
                    this.detected_text_source = None;
                    if this.languages.choose_source(*choice, &mut this.settings) {
                        this.settings.save();
                    }
                    if this.languages.explicit() || this.languages.automatic {
                        this.refresh_target_choices(window, cx);
                    }
                    this.options_changed(window, cx);
                }
            }),
            cx.subscribe_in(&target_select, window, |this, _, event, window, cx| {
                if let SelectEvent::Confirm(Some(choice)) = event {
                    this.detected_text_source = None;
                    let changed = if this.show_setup(cx) {
                        std::mem::replace(&mut this.settings.target_language, *choice) != *choice
                    } else {
                        this.languages.choose_target(*choice, &mut this.settings)
                    };
                    if changed {
                        this.setup_attempt = None;
                        this.settings.save();
                    }
                    if this.languages.automatic {
                        this.refresh_translation_choices(window, cx);
                    }
                    this.options_changed(window, cx);
                }
            }),
            cx.subscribe_in(&ui_language_select, window, |this, _, event, window, cx| {
                if let SelectEvent::Confirm(Some(language)) = event {
                    this.set_ui_language(*language, window, cx);
                }
            }),
            cx.subscribe_in(&theme_select, window, |this, _, event, window, cx| {
                if let SelectEvent::Confirm(Some(theme)) = event {
                    this.set_theme(*theme, window, cx);
                }
            }),
            cx.subscribe_in(&ui_scale_select, window, |this, _, event, window, cx| {
                if let SelectEvent::Confirm(Some(scale)) = event {
                    this.set_ui_scale(*scale, window, cx);
                }
            }),
            cx.subscribe_in(&chunk_select, window, |this, _, event, window, cx| {
                if let SelectEvent::Confirm(Some(tokens)) = event {
                    this.set_document(
                        DocumentSettings {
                            chunk_tokens: *tokens,
                            ..this.settings.document
                        },
                        window,
                        cx,
                    );
                }
            }),
            cx.subscribe_in(&history_select, window, |this, _, event, window, cx| {
                if let SelectEvent::Confirm(Some(tokens)) = event {
                    this.set_document(
                        DocumentSettings {
                            history_tokens: *tokens,
                            ..this.settings.document
                        },
                        window,
                        cx,
                    );
                }
            }),
            cx.observe_window_appearance(window, |this, window, cx| {
                if this.settings.theme == ThemeSetting::System {
                    apply_appearance(&this.settings, window, cx);
                }
            }),
            cx.observe_window_activation(window, |this, window, cx| {
                if window.is_window_active()
                    && (this.settings.models_dir.is_some() || this.show_setup(cx))
                {
                    this.check_models_folder(window, cx);
                }
            }),
            cx.subscribe_in(&style_select, window, |this, _, event, window, cx| {
                if let SelectEvent::Confirm(Some(choice)) = event {
                    this.settings.translation_style = *choice;
                    this.settings.save();
                    this.options_changed(window, cx);
                }
            }),
        ];

        let (tx, rx) = async_channel::unbounded();
        let worker = inference_worker(&tx);
        let events = cx.spawn_in(window, async move |this, cx| {
            while let Ok(first) = rx.recv().await {
                // Take everything that has arrived, so the UI handles a burst of tokens at once.
                let mut batch = vec![first];
                while let Ok(event) = rx.try_recv() {
                    batch.push(event);
                }
                let streaming = batch.iter().any(|e| matches!(e, Event::Token { .. }));
                if this
                    .update_in(cx, |this, window, cx| {
                        this.on_worker_events(batch, window, cx)
                    })
                    .is_err()
                {
                    break;
                }
                if streaming {
                    // Let tokens accumulate; the next batch picks them up.
                    cx.background_executor().timer(STREAM_FRAME).await;
                }
            }
        });

        source.update(cx, |state, cx| state.focus(window, cx));

        let initial = match custom_model {
            Some(path) => Some(ModelChoice::Custom(path)),
            None => startup_model(&settings, &store).map(ModelChoice::Catalog),
        };

        let rollback_model = if settings.license_accepted
            && initial
                .as_ref()
                .is_some_and(|choice| choice.profile().capabilities().directions.is_some())
        {
            store.preferred_plamo().map(ModelChoice::Catalog)
        } else {
            None
        };
        let mut languages = lang::Languages::default();
        languages.automatic =
            !settings.model_fixed && !matches!(initial, Some(ModelChoice::Custom(_)));
        let mut this = Self {
            files: files::FilePanel::default(),
            batch: Default::default(),
            external_engine: Default::default(),
            pending_close: Default::default(),
            picking_files: false,
            file_tab: false,
            jobs: crate::jobs::Jobs::default(),
            source,
            output,
            source_select,
            target_select,
            style_select,
            ui_language_select,
            theme_select,
            ui_scale_select,
            chunk_select,
            history_select,
            languages,
            auto_translate: true,
            context_input,
            glossary_input,
            show_options: false,
            context: String::new(),
            glossary: glossary::parse(&glossary_text_for_parse),
            worker,
            worker_events: tx,
            worker_failed: false,
            models,
            models_folder_was_missing: setup::configured_folder_missing(
                settings.models_dir.as_deref(),
            ),
            backend,
            devices: None,
            loaded_on: None,
            pending_delete: None,
            releases: Releases::default(),
            setup_memory: setup::installed_memory(),
            settings,
            active_model: None,
            rollback_model,
            failed_switch: None,
            model_notice: None,
            model_menu: false,
            file_on_load: None,
            load_for_file: false,
            deferred_model_use: None,
            batch_on_load: None,
            required_model: None,
            required_format: None,
            setup_attempt: None,
            load_recovery: None,
            model_ready: false,
            status: Status::NoModel,
            current_job: None,
            shown: None,
            translation: String::new(),
            translated_with: None,
            translation_utf16: 0,
            source_info: None,
            cache: VecDeque::new(),
            chunks: Vec::new(),
            cuts: Vec::new(),
            next_cut: 0,
            alignment: None,
            source_version: 0,
            shown_version: 0,
            sync: SyncState::default(),
            stream_tokens: 0,
            stream_started: None,
            job_started: None,
            current_chunk: None,
            paste_pending: false,
            detected_text_source: None,
            translate_on_load: false,
            pending_loads: 0,
            runtime_error: None,
            measuring: None,
            measure_requested: false,
            shown_partial: false,
            newer_release: None,
            update_check: UpdateCheck::Idle,
            _update_task: Task::ready(()),
            installation: None,
            install: InstallState::Idle,
            _install_task: Task::ready(()),
            copied: false,
            debounce: Task::ready(()),
            copied_reset: Task::ready(()),
            _subscriptions: subscriptions,
            _events: events,
        };
        this.refresh_translation_choices(window, cx);
        this.link_selection_focus(cx);
        this.check_for_update(UpdateCheckTrigger::Startup, cx);
        // Development aid (debug builds only): open the settings at start, for screenshots.
        if cfg!(debug_assertions)
            && std::env::var_os("PRIVATE_TRANSLATE_DEBUG_OPEN_SETTINGS").is_some()
        {
            cx.defer_in(window, |this, window, cx| this.open_settings(window, cx));
        }
        this.check_babeldoc_engine(cx);
        if let Some(choice) = initial {
            this.load_model(choice, window, cx);
        } else if this.settings.model.is_some() && !this.show_setup(cx) {
            cx.defer_in(window, |this, window, cx| this.open_models(window, cx));
        }
        this
    }

    fn accept_license(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        self.settings.license_accepted = true;
        self.settings.save();
        self.models
            .update(cx, |models, cx| models.set_license_accepted(true, cx));
        self.update_recommendation(cx);
        self.refresh_translation_choices(window, cx);
        cx.notify();
        if matches!(self.status, Status::LicenseRequired) {
            self.start_load(window, cx);
        }
    }

    /// A batch may switch models between rows, after the previous file has released the worker.
    fn can_load_model(&self) -> bool {
        !self.jobs.file_running() && (!self.jobs.busy() || self.batch_on_load.is_some())
    }

    /// Makes `choice` the active model and loads it.
    fn load_model(&mut self, choice: ModelChoice, window: &mut Window, cx: &mut Context<Self>) {
        if !self.can_load_model() {
            return;
        }
        // Another model's translation must not stay to be copied while this one loads, or after
        // it fails to.
        if !self.translation.is_empty() && self.translated_with.as_ref() != Some(&choice) {
            self.set_translation(String::new(), window, cx);
            self.translated_with = None;
        }
        self.file_on_load = None;
        self.active_model = Some(choice);
        self.start_load(window, cx);
    }

    /// Releases the active model. The saved selection stays, so the next start loads it again.
    fn unload_model(&mut self, cx: &mut Context<Self>) {
        if self.jobs.busy() {
            return;
        }
        crate::app_log!("unloading the active model");
        self.request_unload();
        self.active_model = None;
        self.load_recovery = None;
        self.rollback_model = None;
        self.failed_switch = None;
        self.model_notice = None;
        self.model_ready = false;
        self.translate_on_load = false;
        // A file waiting for a switch must not start on whatever model loads next.
        self.file_on_load = None;
        self.load_for_file = false;
        self.current_job = None;
        self.shown = None;
        self.loaded_on = None;
        self.cache.clear();
        self.status = Status::Unloaded;
        self.models
            .update(cx, |models, cx| models.set_active(None, cx));
        self.backend
            .update(cx, |backend, cx| backend.set_model_bytes(None, cx));
        cx.notify();
    }

    /// Asks the worker to release its model; `Event::Unloaded` answers.
    fn request_unload(&mut self) {
        self.worker.unload();
        self.releases.requested();
    }

    /// (Re)loads the active model with the current backend settings. Before llama.cpp has
    /// reported its devices this only marks the model as loading; the devices event retries.
    fn start_load(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if !self.can_load_model() {
            return;
        }
        let Some(choice) = self.active_model.clone() else {
            return;
        };
        if choice.profile() == ModelProfile::PlamoPfml && !self.settings.license_accepted {
            self.status = Status::LicenseRequired;
            cx.notify();
            return;
        }
        self.load_recovery = None;
        self.apply_model_profile(choice.profile(), window, cx);
        self.current_job = None;
        self.shown = None;
        self.model_ready = false;
        // Cached results belong to the previous model or settings.
        self.cache.clear();
        self.loaded_on = None;
        self.status = Status::Loading(0.0);
        cx.notify();

        let path = match &choice {
            ModelChoice::Catalog(entry) => self.models.read(cx).store().path(entry),
            ModelChoice::Custom(path) => path.clone(),
        };
        if !path.is_file() {
            self.request_unload();
            self.model_load_failed(
                t!("status.model_missing", path = path.display().to_string()).into_owned(),
                LoadFailure::Other,
                window,
                cx,
            );
            return;
        }
        let bytes = std::fs::metadata(&path).ok().map(|m| m.len());
        self.backend
            .update(cx, |backend, cx| backend.set_model_bytes(bytes, cx));
        if self.worker_failed {
            // The previous runtime has shut down, so a new worker can start.
            self.devices = None;
            self.worker_failed = false;
            self.worker = inference_worker(&self.worker_events);
        }
        let Some(devices) = &self.devices else {
            return;
        };
        // The measurement's result decides the device; it loads the model afterwards.
        if let Some(first) = self.measuring {
            self.status = Status::Measuring {
                first,
                step: 0,
                count: 0,
            };
            return;
        }
        let measure = if self.measure_requested {
            Some((false, backend::candidates(devices)))
        } else {
            backend::first_measurement(&self.settings.backend, devices).map(|c| (true, c))
        };
        if let Some((first, candidates)) = measure {
            crate::app_log!(
                "measuring {} device(s) with {}",
                candidates.len(),
                path.display()
            );
            self.measure_requested = false;
            self.measuring = Some(first);
            self.status = Status::Measuring {
                first,
                step: 0,
                count: candidates.len(),
            };
            self.backend
                .update(cx, |backend, cx| backend.set_measuring(true, cx));
            self.worker.measure(MeasureRequest {
                path,
                entry: choice.entry(),
                devices: candidates,
            });
            return;
        }
        let device = backend::resolve(
            &self.settings.backend.device,
            self.settings.backend.measured.as_ref(),
            devices,
        );
        self.loaded_on = Some(backend::device_label(device));
        crate::app_log!(
            "loading {} on {}",
            path.display(),
            backend::device_label(device)
        );
        #[cfg(private_translate_bundled_metal)]
        if device.is_some_and(|device| device.name.starts_with("MTL"))
            && let Some(selection) = crate::macos::metal_shader_selection()
        {
            crate::app_log!("{selection}");
        }
        self.pending_loads += 1;
        let mut session = backend::session_options(device);
        if let Some(limit) = choice.profile().capabilities().context_limit {
            session.n_ctx = limit;
        }
        self.worker.load(LoadRequest {
            path,
            entry: choice.entry(),
            model: backend::model_options(device),
            session,
        });
    }

    fn model_load_failed(
        &mut self,
        error: String,
        kind: LoadFailure,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.loaded_on = None;
        self.translate_on_load = false;
        self.file_on_load = None;
        self.load_for_file = false;
        let Some(choice) = self.active_model.clone() else {
            return;
        };
        let label = choice.label();
        crate::app_log!("model load failed: {label}: {error}");
        let failed_entry = choice.entry();
        let failure = LoadRecovery {
            model: choice,
            error,
            kind,
        };
        let message = failure.message();
        self.models.update(cx, |models, cx| {
            models.set_active(None, cx);
            if let Some(entry) = failed_entry {
                models.record_load_failure(
                    entry,
                    message.clone(),
                    kind == LoadFailure::InvalidFile,
                    cx,
                );
            }
        });
        if let Some(prior) = self.rollback_model.take() {
            crate::app_log!("restoring previous model: {}", prior.label());
            self.failed_switch = Some(format!("{label}: {message}"));
            self.load_model(prior, window, cx);
        } else {
            if let Some(id) = self.batch_on_load.take() {
                self.batch_load_failed(id, &message, window, cx);
            }
            self.failed_switch = None;
            if self.languages.automatic {
                self.active_model = None;
            }
            if !self.settings.first_run_setup_pending {
                self.required_model = None;
                self.load_recovery = Some(failure);
            }
            self.status = Status::Error(message);
            if self.show_setup(cx) {
                self.refresh_target_choices(window, cx);
            }
        }
        cx.notify();
    }

    fn capabilities(&self) -> &'static Capabilities {
        file_capabilities(
            self.active_model.as_ref(),
            &self.settings,
            self.languages.profile,
        )
    }

    fn requires_language_pair(&self) -> bool {
        self.languages.explicit()
    }

    /// Offers the language choices of the loaded model's `profile` (see [`lang::Languages`]).
    /// Nothing is saved: each kind of model gets back what was last chosen with it.
    fn apply_model_profile(
        &mut self,
        profile: ModelProfile,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.languages.load(profile, &self.settings);
        // Codes only, so reports show what a model started with.
        crate::app_log!(
            "languages: {} → {}",
            self.languages.source.code().unwrap_or("auto"),
            match self.languages.target(&self.settings) {
                TargetChoice::Lang(code) => code,
                TargetChoice::JapaneseEnglish => "ja/en",
            }
        );
        self.refresh_translation_choices(window, cx);
    }

    fn system_memory(&self) -> u64 {
        self.devices
            .as_deref()
            .unwrap_or_default()
            .iter()
            .find(|device| device.kind == engine::llama::DeviceKind::Cpu)
            .map_or(0, |device| device.memory_total)
    }

    fn current_pair(&self) -> (Option<&'static str>, Option<&'static str>) {
        let source = self.languages.source.code();
        if source.is_none()
            && self.languages.target(&self.settings) == TargetChoice::JapaneseEnglish
        {
            return (Some("en"), Some("ja"));
        }
        let target = match self.languages.target(&self.settings) {
            TargetChoice::Lang(code) => Some(code),
            TargetChoice::JapaneseEnglish => {
                source.map(|code| TargetChoice::JapaneseEnglish.resolve_for(code == "ja"))
            }
        };
        (source, target)
    }

    fn model_selection(
        &self,
        format: Option<engine::profiles::FileFormat>,
        cx: &App,
    ) -> selection::Selection {
        let (source, target) = self.current_pair();
        self.select_pair(source, target, format, cx)
    }

    fn select_pair(
        &self,
        source: Option<&str>,
        target: Option<&str>,
        format: Option<engine::profiles::FileFormat>,
        cx: &App,
    ) -> selection::Selection {
        self.select_pair_from(source, target, format, &catalog::entries(), cx)
    }

    fn select_pair_from(
        &self,
        source: Option<&str>,
        target: Option<&str>,
        format: Option<engine::profiles::FileFormat>,
        entries: &[&'static CatalogEntry],
        cx: &App,
    ) -> selection::Selection {
        self.models.read(cx).select_automatically(
            selection::Request {
                source,
                target,
                format,
                system_memory: self.system_memory(),
                quality: if self.settings.model_high_quality {
                    QualityTier::High
                } else {
                    QualityTier::Standard
                },
                plamo_consent: self.settings.license_accepted,
                bidirectional: self.languages.source == SourceChoice::Detect
                    && self.languages.target(&self.settings) == TargetChoice::JapaneseEnglish
                    && matches!(
                        (source, target),
                        (Some("en"), Some("ja")) | (Some("ja"), Some("en"))
                    ),
                selected: self.settings.model.as_deref(),
            },
            entries,
        )
    }

    fn update_recommendation(&mut self, cx: &mut Context<Self>) {
        let recommended = if self.show_setup(cx) {
            setup::recommend(
                self.setup_request(self.file_tab),
                &catalog::entries(),
                |_| true,
            )
        } else {
            let result = self.model_selection(None, cx);
            result
                .suggestion
                .map(|suggestion| suggestion.entry)
                .or(result.usable)
        };
        let system_memory = self.system_memory();
        let recommended = recommended.filter(|entry| {
            !self.file_tab
                || entry.id() != catalog::HY_MT2.id()
                || !selection::fits_memory_budget(
                    catalog::HY_MT2_7B.bytes,
                    system_memory.max(self.setup_memory),
                )
        });
        self.models.update(cx, |models, cx| {
            models.set_recommended(recommended, system_memory, cx);
        });
    }

    fn on_backend_event(
        &mut self,
        _: &Entity<BackendPanel>,
        event: &BackendEvent,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if keeps_model(&self.jobs) {
            // Not reachable from the models dialog, which a file job disables; the selector
            // shows the device in use again all the same.
            let settings = self.settings.backend.clone();
            self.backend
                .update(cx, |backend, cx| backend.set_settings(settings, window, cx));
            return;
        }
        match event {
            BackendEvent::Changed(device) => {
                self.settings.backend.device = device.clone();
                self.settings.save();
                self.update_recommendation(cx);
            }
            BackendEvent::Measure => self.measure_requested = true,
        }
        self.start_load(window, cx);
    }

    fn on_models_event(
        &mut self,
        _: &Entity<ModelsPanel>,
        event: &ModelsEvent,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        match event {
            ModelsEvent::CatalogUpdated => self.update_recommendation(cx),
            // Not asked for by a click: a download finished its check.
            ModelsEvent::Verified(entry) => self.install_model_update(entry, cx),
            ModelsEvent::AcceptLicense => self.accept_license(window, cx),
            ModelsEvent::UseOnce(entry) if keeps_model(&self.jobs) => {
                self.deferred_model_use = Some(entry);
            }
            _ if keeps_model(&self.jobs) => {}
            ModelsEvent::Downloaded(entry) => {
                let models = self.models.read(cx);
                if !self.model_ready
                    && !self.setup_loading()
                    && models.use_downloaded_for_setup(entry, self.settings.first_run_setup_pending)
                {
                    self.setup_attempt = Some(entry);
                    self.load_model(ModelChoice::Catalog(entry), window, cx);
                }
            }
            ModelsEvent::Use(entry) | ModelsEvent::UseOnce(entry)
                if entry.is_plamo() && !self.settings.license_accepted => {}
            ModelsEvent::Use(entry) | ModelsEvent::UseOnce(entry) => {
                self.model_notice = None;
                let explicit = matches!(event, ModelsEvent::Use(_));
                if explicit {
                    self.pin_model(entry, window, cx);
                }
                self.load_for_file = !explicit && self.required_format.is_some();
                self.models.update(cx, |models, _| models.clear_auto_use());
                let choice = ModelChoice::Catalog(entry);
                self.remember_rollback(&choice);
                if self.settings.first_run_setup_pending || self.show_setup(cx) {
                    self.setup_attempt = Some(entry);
                }
                if !self.model_ready || self.active_model.as_ref() != Some(&choice) {
                    self.load_model(choice, window, cx);
                }
                if window.has_active_dialog(cx) {
                    window.close_dialog(cx);
                }
            }
            ModelsEvent::Unload => self.unload_model(cx),
            ModelsEvent::DeleteActive(entry) => {
                // The file stays open until the worker drops the model (and cannot be deleted
                // on Windows before that), so delete it when `Event::Unloaded` arrives.
                self.pending_delete = Some(entry);
                self.unload_model(cx);
            }
            ModelsEvent::ChangeFolder(dir) => self.change_models_folder(dir.clone(), window, cx),
        }
    }

    /// Installs a verified download (see [`ReplaceStep`]). A file job keeps the model, so the
    /// update then waits for it to end (see [`Self::install_deferred_model_updates`]).
    fn install_model_update(&mut self, entry: &'static CatalogEntry, cx: &mut Context<Self>) {
        match ReplaceStep::of(keeps_model(&self.jobs), self.active_model.as_ref(), entry) {
            ReplaceStep::Wait => {
                crate::app_log!("the update of {} waits for the file job", entry.id());
                self.releases.defer(entry);
            }
            ReplaceStep::Reload => {
                // As for deleting: Windows cannot replace the file while it is loaded.
                crate::app_log!(
                    "releasing the model to install the update of {}",
                    entry.id()
                );
                self.unload_model(cx);
                self.releases.wait_for_last(entry);
            }
            ReplaceStep::Install => {
                self.models
                    .update(cx, |models, cx| models.install_verified(entry, cx));
            }
        }
    }

    fn install_deferred_model_updates(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if !keeps_model(&self.jobs) {
            for entry in self.releases.take_deferred() {
                self.install_model_update(entry, cx);
            }
            if let Some(entry) = self.deferred_model_use.take() {
                self.on_models_event(
                    &self.models.clone(),
                    &ModelsEvent::UseOnce(entry),
                    window,
                    cx,
                );
            }
            if self.models_folder_was_missing
                && !setup::configured_folder_missing(self.settings.models_dir.as_deref())
            {
                self.rescan_models_folder(false, window, cx);
            }
        }
    }

    /// Keeps models in `dir` (the default folder for `None`). The files stay where they are. The
    /// model loaded from the old folder gives way to what the new one has, chosen as at start.
    fn change_models_folder(
        &mut self,
        dir: Option<PathBuf>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        // The default folder, picked, stays the default (wherever the data directory moves).
        let dir = dir.filter(|dir| *dir != settings::default_models_dir());
        let changed = self.settings.models_dir != dir;
        if changed {
            self.settings.models_dir = dir;
            self.settings.save();
            crate::app_log!(
                "models folder set to {}",
                self.settings.models_dir().display()
            );
        }
        self.rescan_models_folder(changed, window, cx);
    }

    fn check_models_folder(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if setup::configured_folder_missing(self.settings.models_dir.as_deref()) {
            self.models_folder_was_missing = true;
        } else if self.models_folder_was_missing {
            self.rescan_models_folder(false, window, cx);
        }
        cx.notify();
    }

    fn rescan_models_folder(&mut self, changed: bool, window: &mut Window, cx: &mut Context<Self>) {
        if !changed
            && (!self.can_load_model()
                || self.setup_loading()
                || self.pending_loads > 0
                || self.models.read(cx).setup_download().is_some())
        {
            return;
        }
        self.models_folder_was_missing =
            setup::configured_folder_missing(self.settings.models_dir.as_deref());
        let store = ModelStore::new(self.settings.models_dir());
        self.models
            .update(cx, |models, cx| models.set_store(store.clone(), cx));
        if matches!(self.active_model, Some(ModelChoice::Custom(_))) {
            cx.notify();
            return;
        }
        self.rollback_model = None;
        self.model_notice = None;
        self.setup_attempt = None;
        self.load_recovery = None;
        let choice = if changed {
            model_to_load(&self.settings, &store)
        } else {
            startup_model(&self.settings, &store)
        };
        match choice.map(ModelChoice::Catalog) {
            Some(choice)
                if changed || self.active_model.as_ref() != Some(&choice) || !self.model_ready =>
            {
                self.load_model(choice, window, cx);
            }
            Some(_) => {}
            None if self.active_model.is_some() => self.unload_model(cx),
            None => self.status = Status::NoModel,
        }
        if self.show_setup(cx) {
            self.refresh_target_choices(window, cx);
        }
        cx.notify();
    }

    fn open_settings(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        // A result shows only after Check now, not from the check at start.
        if self.update_check.checking_key().is_none() {
            self.update_check = UpdateCheck::Idle;
        }
        let language = self.ui_language_select.clone();
        let theme = self.theme_select.clone();
        let ui_scale = self.ui_scale_select.clone();
        let chunk = self.chunk_select.clone();
        let history = self.history_select.clone();
        let view = cx.entity().downgrade();
        window.open_dialog(cx, move |dialog, _, cx| {
            // Read on every render: the theme can change from this dialog.
            let muted = cx.theme().muted_foreground;
            let (sync_scroll, check_updates, update_check) = view
                .upgrade()
                .map(|view| {
                    let this = view.read(cx);
                    (
                        this.settings.sync_scroll,
                        this.settings.check_updates,
                        this.update_check.clone(),
                    )
                })
                .unwrap_or((false, false, UpdateCheck::Idle));
            let (double_copy, resident) = view
                .upgrade()
                .map(|view| {
                    let settings = &view.read(cx).settings;
                    (settings.double_copy, settings.resident)
                })
                .unwrap_or_default();
            let (babeldoc_engine, babeldoc_status) = view
                .upgrade()
                .map(|view| {
                    let this = view.read(cx);
                    (
                        this.settings.babeldoc_engine.clone(),
                        this.external_engine.status().clone(),
                    )
                })
                .unwrap_or_default();
            let babeldoc_view = view.clone();
            let watch_error = crate::shell::watch_error(cx);
            let toggle_view = view.clone();
            let double_copy_view = view.clone();
            let resident_view = view.clone();
            let updates_view = view.clone();
            let check_view = view.clone();
            let row = |label: &str, select: AnyElement| {
                h_flex()
                    .gap_3()
                    .text_sm()
                    .child(div().w(scaled(140.)).child(t!(label).into_owned()))
                    .child(select)
            };
            let select = |state: &Entity<_>| Select::new(state).w(scaled(260.)).into_any_element();
            dialog
                .title(t!("settings.title").into_owned())
                .w(scaled_px(600., cx))
                .child(
                    v_flex()
                        .gap_3()
                        .child(row(
                            "settings.language",
                            Select::new(&language).w(scaled(260.)).into_any_element(),
                        ))
                        .child(row(
                            "settings.theme",
                            Select::new(&theme).w(scaled(260.)).into_any_element(),
                        ))
                        .child(row(
                            "settings.ui_scale",
                            Select::new(&ui_scale).w(scaled(260.)).into_any_element(),
                        ))
                        .child(
                            div()
                                .text_xs()
                                .text_color(muted)
                                .child(t!("settings.ui_scale_note").into_owned()),
                        )
                        .child(row("settings.chunk", select(&chunk)))
                        .child(row("settings.history", select(&history)))
                        .child(
                            div()
                                .text_xs()
                                .text_color(muted)
                                .child(t!("settings.document_note").into_owned()),
                        )
                        .children(files::external_engine_row(
                            babeldoc_engine,
                            babeldoc_status,
                            muted,
                            babeldoc_view,
                        ))
                        .child(
                            Switch::new("sync-scroll")
                                .color(rgb(ACTION_ACCENT))
                                .checked(sync_scroll)
                                .label(t!("settings.sync_scroll").into_owned())
                                .on_click(move |checked: &bool, window, cx| {
                                    let _ = toggle_view
                                        .update(cx, |this, cx| this.set_sync_scroll(*checked, cx));
                                    window.refresh();
                                }),
                        )
                        .child(
                            div()
                                .text_xs()
                                .text_color(muted)
                                .child(t!("settings.sync_scroll_note").into_owned()),
                        )
                        .child(
                            Switch::new("double-copy")
                                .color(rgb(ACTION_ACCENT))
                                .checked(double_copy)
                                .label(t!("settings.double_copy").into_owned())
                                .on_click(move |checked: &bool, window, cx| {
                                    let _ = double_copy_view
                                        .update(cx, |this, cx| this.set_double_copy(*checked, cx));
                                    window.refresh();
                                }),
                        )
                        .child(
                            div()
                                .text_xs()
                                .text_color(muted)
                                .child(t!("settings.double_copy_note").into_owned()),
                        )
                        .children(watch_error.map(|message| {
                            div().text_xs().text_color(cx.theme().danger).child(message)
                        }))
                        .child(
                            Switch::new("resident")
                                .color(rgb(ACTION_ACCENT))
                                .checked(resident)
                                .label(t!("settings.resident").into_owned())
                                .on_click(move |checked: &bool, window, cx| {
                                    let _ = resident_view
                                        .update(cx, |this, cx| this.set_resident(*checked, cx));
                                    window.refresh();
                                }),
                        )
                        .child(
                            div()
                                .text_xs()
                                .text_color(muted)
                                .child(t!(RESIDENT_NOTE).into_owned()),
                        )
                        .when(RELEASE_UPDATE_CHECK_ENABLED, |panel| {
                            panel
                                .child(
                                    Switch::new("check-updates")
                                        .color(rgb(ACTION_ACCENT))
                                        .checked(check_updates)
                                        .label(t!("settings.check_updates").into_owned())
                                        .on_click(move |checked: &bool, window, cx| {
                                            let _ = updates_view.update(cx, |this, cx| {
                                                this.set_check_updates(*checked, cx)
                                            });
                                            window.refresh();
                                        }),
                                )
                                .child(
                                    div()
                                        .text_xs()
                                        .text_color(muted)
                                        .child(t!("settings.check_updates_note").into_owned()),
                                )
                                .child(update_check_row(update_check, muted, check_view))
                        })
                        .child(
                            Button::new("about")
                                .ghost()
                                .label(t!("about.title").into_owned())
                                .on_click(|_, window, cx| Self::open_about(window, cx)),
                        ),
                )
        });
    }

    pub(crate) fn open_about(window: &mut Window, cx: &mut App) {
        window.open_dialog(cx, |dialog, _, _| {
            dialog.title(t!("about.title").into_owned()).child(
                v_flex()
                    .gap_2()
                    .text_sm()
                    .child(format!("{} {}", t!("app.title"), env!("CARGO_PKG_VERSION")))
                    .child(t!("about.subtitle").into_owned())
                    .child("MIT / Apache License 2.0"),
            )
        });
    }

    fn set_document(
        &mut self,
        document: DocumentSettings,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if self.settings.document == document {
            return;
        }
        self.settings.document = document;
        self.settings.save();
        crate::app_log!(
            "long texts: chunks of {} tokens, history of {} tokens",
            document.chunk_tokens,
            document.history_tokens
        );
        self.options_changed(window, cx);
    }

    fn set_theme(&mut self, theme: ThemeSetting, window: &mut Window, cx: &mut Context<Self>) {
        if self.settings.theme == theme {
            return;
        }
        self.settings.theme = theme;
        self.settings.save();
        crate::app_log!("theme set to {theme:?}");
        apply_appearance(&self.settings, window, cx);
    }

    fn set_ui_scale(&mut self, scale: u16, window: &mut Window, cx: &mut Context<Self>) {
        if self.settings.ui_scale == scale {
            return;
        }
        self.settings.ui_scale = scale;
        self.settings.save();
        crate::app_log!("UI size set to {scale}%");
        apply_appearance(&self.settings, window, cx);
    }

    fn set_ui_language(
        &mut self,
        language: UiLanguage,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if self.settings.ui_language == language {
            return;
        }
        self.settings.ui_language = language;
        self.settings.save();
        let locale = language.locale();
        crate::app_log!("UI language set to {language:?} ({locale})");
        rust_i18n::set_locale(locale);
        self.refresh_locale(window, cx);
        crate::shell::refresh_tray(cx);
        #[cfg(target_os = "macos")]
        crate::macos::set_menu(cx);
    }

    fn installed_profiles(&self, cx: &App) -> Vec<ModelProfile> {
        let models = self.models.read(cx);
        let mut profiles = Vec::new();
        for entry in catalog::entries() {
            if !profiles.contains(&entry.profile)
                && (!entry.is_plamo() || self.settings.license_accepted)
                && models.available_automatically(entry)
            {
                profiles.push(entry.profile);
            }
        }
        profiles
    }

    fn refresh_translation_choices(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let mut sources = self.languages.source_choices();
        let profiles = self
            .languages
            .automatic
            .then(|| self.installed_profiles(cx));
        if let Some(profiles) = &profiles {
            let target = self.languages.target(&self.settings);
            sources.sort_by_cached_key(|choice| {
                choice.value().code().is_some_and(|source| {
                    !profiles.iter().any(|&profile| {
                        selection::supports_direction(
                            profile,
                            Some(source),
                            Some(target.resolve_for(source == "ja")),
                        )
                    })
                })
            });
        }
        if let Some(detect) = sources
            .iter_mut()
            .find(|choice| *choice.value() == SourceChoice::Detect)
        {
            let view = cx.entity().downgrade();
            detect.set_trigger_title(cx.new(|_| DetectedSourceTitle(view)).into());
        }
        let source = self.languages.source;
        self.source_select.update(cx, |state, cx| {
            state.set_items(SearchableVec::new(sources), window, cx);
            state.set_selected_value(&source, window, cx);
        });
        self.set_target_choices(profiles.as_deref(), window, cx);
    }

    fn refresh_target_choices(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let profiles = self
            .languages
            .automatic
            .then(|| self.installed_profiles(cx));
        self.set_target_choices(profiles.as_deref(), window, cx);
    }

    fn set_target_choices(
        &mut self,
        profiles: Option<&[ModelProfile]>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let setting_up = self.show_setup(cx);
        let mut languages = self.languages;
        if setting_up {
            languages.automatic = true;
        }
        let mut choices = languages.target_choices();
        if setting_up {
            choices.retain(|choice| matches!(choice.value(), TargetChoice::Lang(_)));
        }
        if let Some(profiles) = profiles {
            let source = self.languages.source.code();
            choices.sort_by_cached_key(|choice| {
                let target = choice.value().resolve_for(source == Some("ja"));
                !profiles.iter().any(|&profile| {
                    if let (None, Some(directions)) = (source, profile.capabilities().directions) {
                        directions.iter().any(|&(_, to)| to == target)
                    } else {
                        selection::supports_direction(profile, source, Some(target))
                    }
                })
            });
        }
        let target = if setting_up {
            TargetChoice::Lang(self.setup_request(self.file_tab).target)
        } else {
            self.languages.target(&self.settings)
        };
        self.target_select.update(cx, |state, cx| {
            state.set_items(SearchableVec::new(choices), window, cx);
            state.set_selected_value(&target, window, cx);
        });
    }

    /// Rebuilds the strings that were baked into child components when they were created.
    /// Everything else is translated at render time.
    fn refresh_locale(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        self.refresh_translation_choices(window, cx);

        let style = self.settings.translation_style;
        self.style_select.update(cx, |state, cx| {
            state.set_items(lang::style_choices(), window, cx);
            state.set_selected_value(&style, window, cx);
        });
        let language = self.settings.ui_language;
        self.ui_language_select.update(cx, |state, cx| {
            state.set_items(lang::ui_language_choices(), window, cx);
            state.set_selected_value(&language, window, cx);
        });
        let theme = self.settings.theme;
        self.theme_select.update(cx, |state, cx| {
            state.set_items(lang::theme_choices(), window, cx);
            state.set_selected_value(&theme, window, cx);
        });
        let ui_scale = self.settings.ui_scale;
        self.ui_scale_select.update(cx, |state, cx| {
            state.set_items(lang::ui_scale_choices(ui_scale), window, cx);
            state.set_selected_value(&ui_scale, window, cx);
        });
        let document = self.settings.document;
        let (chunk_choices, history_choices) = token_choices(&document);
        self.chunk_select.update(cx, |state, cx| {
            state.set_items(chunk_choices, window, cx);
            state.set_selected_value(&document.chunk_tokens, window, cx);
        });
        self.history_select.update(cx, |state, cx| {
            state.set_items(history_choices, window, cx);
            state.set_selected_value(&document.history_tokens, window, cx);
        });
        for (input, key) in [
            (&self.source, "source.placeholder"),
            (&self.context_input, "options.context_placeholder"),
            (&self.glossary_input, "options.glossary_placeholder"),
        ] {
            let text = t!(key).into_owned();
            input.update(cx, |state, cx| state.set_placeholder(text, window, cx));
        }
        self.backend
            .update(cx, |backend, cx| backend.refresh_locale(window, cx));
        self.models.update(cx, |_, cx| cx.notify());
        cx.notify();
    }

    fn all_installed_models_failed(&self, cx: &App) -> bool {
        let models = self.models.read(cx);
        let entries = catalog::entries();
        let mut installed = entries
            .iter()
            .filter(|entry| {
                (!entry.is_plamo() || self.settings.license_accepted)
                    && models.store().state(entry).is_usable()
            })
            .peekable();
        installed.peek().is_some() && installed.all(|entry| !models.available_automatically(entry))
    }

    fn show_setup(&self, cx: &App) -> bool {
        let loading = self.setup_loading();
        self.models_folder_missing()
            || self.load_recovery.is_some()
            || (loading && self.setup_attempt.is_some())
            || (!self.can_reload_after_runtime_error()
                && !matches!(self.status, Status::LicenseRequired)
                && setup::show_card(
                    self.model_ready || (!loading && self.models.read(cx).has_usable_model(None)),
                    loading,
                ))
    }

    fn can_reload_after_runtime_error(&self) -> bool {
        self.worker_failed && self.active_model.is_some() && matches!(self.status, Status::Error(_))
    }

    fn models_folder_missing(&self) -> bool {
        self.models_folder_was_missing
            || setup::configured_folder_missing(self.settings.models_dir.as_deref())
    }

    fn setup_loading(&self) -> bool {
        matches!(self.status, Status::Loading(_) | Status::Measuring { .. })
    }

    fn setup_busy(&self, cx: &App) -> bool {
        self.show_setup(cx)
            && (self.setup_loading() || self.models.read(cx).setup_download().is_some())
    }

    fn setup_request(&self, file_tab: bool) -> setup::Request<'static> {
        let memory = if self.setup_memory > 0 {
            self.setup_memory
        } else {
            self.system_memory()
        };
        setup::Request {
            target: self.settings.target_language.resolve_for(false),
            format: file_tab.then_some(
                self.required_format
                    .unwrap_or(engine::profiles::FileFormat::Word),
            ),
            memory,
            quality: if self.settings.model_high_quality {
                QualityTier::High
            } else {
                QualityTier::Standard
            },
            consent: self.settings.license_accepted,
        }
    }

    fn prepare_first_model(
        &mut self,
        entry: &'static CatalogEntry,
        disk_blocked: Option<&'static CatalogEntry>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if !self.show_setup(cx) || self.setup_busy(cx) || self.models_folder_missing() {
            cx.notify();
            return;
        }
        let models = self.models.read(cx);
        let free = setup::available_space(models.store().dir()).ok();
        // If the larger recommendation fits again, show it before the user commits to it.
        if disk_blocked.is_some_and(|original| {
            original != entry && setup::preflight(models.store(), original, free).is_ok()
        }) || (!models.setup_ready(entry)
            && setup::blocks_download(setup::preflight(models.store(), entry, free)))
        {
            cx.notify();
            return;
        }
        self.setup_attempt = Some(entry);
        self.load_recovery = None;
        if entry.is_plamo() && !self.settings.license_accepted {
            self.accept_license(window, cx);
        }
        self.models
            .update(cx, |models, cx| models.start_setup(entry, cx));
    }

    fn render_model_recovery(&self, missing_folder: bool, cx: &Context<Self>) -> impl IntoElement {
        let theme = cx.theme();
        let models = self.models.read(cx);
        let panel = v_flex()
            .w_full()
            .max_w(scaled(540.))
            .m_auto()
            .flex_shrink_0()
            .rounded_lg()
            .border_1()
            .border_color(theme.border)
            .p_5()
            .gap_3();
        let panel = if missing_folder {
            let reason = t!("models.folder_missing").into_owned();
            panel
                .child(div().font_semibold().child(reason))
                .when_some(models.folder_picker_error(), |panel, error| {
                    panel.child(
                        div()
                            .text_sm()
                            .text_color(theme.danger)
                            .child(t!("models.folder_failed", message = error).into_owned()),
                    )
                })
                .child(
                    h_flex()
                        .flex_wrap()
                        .gap_2()
                        .child(
                            files::file_action(
                                Button::new("recover-models-folder")
                                    .self_start()
                                    .label(t!("models.select_folder").into_owned()),
                                false,
                                cx,
                            )
                            .disabled(models.setup_download().is_some())
                            .on_click(cx.listener(|this, _, _, cx| {
                                this.models
                                    .update(cx, |models, cx| models.choose_folder(cx));
                            })),
                        )
                        .child(
                            Button::new("check-models-folder")
                                .label(t!("models.folder_check_again").into_owned())
                                .disabled(models.setup_download().is_some())
                                .on_click(cx.listener(|this, _, window, cx| {
                                    this.check_models_folder(window, cx);
                                })),
                        ),
                )
        } else if let Some(failure) = &self.load_recovery {
            let alternative = failure.model.entry().and_then(|failed| {
                let mut free = None;
                let (source, target) = self.current_pair();
                let mut request = self.setup_request(self.file_tab);
                if let Some(target) = target {
                    request.target = target;
                }
                setup::load_alternative(
                    failure.kind,
                    request,
                    &catalog::entries(),
                    failed,
                    |entry| {
                        if !selection::supports_direction(
                            entry.profile,
                            source,
                            Some(request.target),
                        ) {
                            return None;
                        }
                        match models.store().state(entry) {
                            state
                                if state.is_usable()
                                    && models.available_automatically(entry)
                                    && (!entry.is_plamo() || self.settings.license_accepted) =>
                            {
                                Some(setup::Alternative::Installed)
                            }
                            LocalState::Missing | LocalState::Partial(_) => {
                                let available = *free.get_or_insert_with(|| {
                                    setup::available_space(models.store().dir()).ok()
                                });
                                setup::preflight(models.store(), entry, available)
                                    .is_ok()
                                    .then_some(setup::Alternative::Download)
                            }
                            _ => None,
                        }
                    },
                )
            });
            let model = failure.model.name();
            panel
                .child(
                    div()
                        .font_semibold()
                        .child(t!("models.load_failed", model = model).into_owned()),
                )
                .child(
                    div()
                        .text_sm()
                        .text_color(theme.danger)
                        .child(failure.message()),
                )
                .child(
                    h_flex()
                        .flex_wrap()
                        .gap_2()
                        .child(
                            files::file_action(
                                Button::new("retry-failed-load")
                                    .label(t!("models.retry").into_owned()),
                                false,
                                cx,
                            )
                            .on_click(cx.listener(
                                |this, _, window, cx| {
                                    if let Some(failure) = &this.load_recovery {
                                        this.load_model(failure.model.clone(), window, cx);
                                    }
                                },
                            )),
                        )
                        .child(
                            Button::new("open-models-after-load")
                                .label(t!("models.title").into_owned())
                                .on_click(
                                    cx.listener(|this, _, window, cx| this.open_models(window, cx)),
                                ),
                        )
                        .when_some(alternative, |row, (entry, action)| {
                            let name = ModelChoice::Catalog(entry).name();
                            let needs_consent = action == setup::Alternative::Download
                                && entry.is_plamo()
                                && !self.settings.license_accepted;
                            let label = match action {
                                setup::Alternative::Installed => {
                                    t!("models.use_named", model = name)
                                }
                                setup::Alternative::Download if needs_consent => {
                                    t!("models.agree_download_named", model = name)
                                }
                                setup::Alternative::Download => {
                                    t!("models.download_named", model = name)
                                }
                            };
                            row.child(
                                Button::new("smaller-model-after-load")
                                    .label(label.into_owned())
                                    .on_click(cx.listener(
                                        move |this, _, window, cx| {
                                            match action {
                                                setup::Alternative::Installed => {
                                                    if this
                                                        .models
                                                        .read(cx)
                                                        .available_automatically(entry)
                                                    {
                                                        this.load_model(
                                                            ModelChoice::Catalog(entry),
                                                            window,
                                                            cx,
                                                        );
                                                    } else {
                                                        cx.notify();
                                                    }
                                                }
                                                setup::Alternative::Download => {
                                                    this.prepare_first_model(
                                                        entry, None, window, cx,
                                                    );
                                                }
                                            }
                                        },
                                    )),
                            )
                            .when(needs_consent, |row| {
                                row.child(crate::models::model_license_button(entry))
                            })
                        }),
                )
        } else {
            panel
        };
        v_flex()
            .id("model-recovery-scroll")
            .flex_1()
            .min_h_0()
            .p_3()
            .overflow_y_scroll()
            .child(panel)
    }

    fn render_setup(&self, cx: &Context<Self>) -> impl IntoElement {
        let theme = cx.theme();
        let models = self.models.read(cx);
        let busy = self.setup_busy(cx);
        let entries = catalog::entries();
        let request = self.setup_request(self.file_tab);
        let entry = self
            .setup_attempt
            .filter(|entry| models.setup_progress(entry).is_some())
            .or_else(|| models.setup_download())
            .or(self.setup_attempt)
            .or_else(|| setup::recommend(request, &entries, |_| true));
        let select = Select::new(&self.target_select)
            .w_full()
            .disabled(busy)
            .menu_width(scaled(260.))
            .search_placeholder(t!("search.language").into_owned());

        let scroller = v_flex()
            .flex_1()
            .min_h_0()
            .px_3()
            .py_3()
            .id("setup-scroll")
            .overflow_y_scroll();
        scroller.child(
            v_flex()
                .w_full()
                .max_w(scaled(540.))
                .m_auto()
                .flex_shrink_0()
                .rounded_lg()
                .border_1()
                .border_color(theme.border)
                .p_5()
                .gap_3()
                .child(
                    v_flex()
                        .gap_1()
                        .child(
                            div()
                                .text_lg()
                                .font_semibold()
                                .child(t!("setup.title").into_owned()),
                        )
                        .child(
                            div().text_sm().child(
                                t!(if cfg!(target_os = "macos") {
                                    "setup.about_mac"
                                } else {
                                    "setup.about_computer"
                                })
                                .into_owned(),
                            ),
                        ),
                )
                .child(
                    v_flex()
                        .gap_1()
                        .child(
                            div()
                                .font_semibold()
                                .child(t!("setup.target_language").into_owned()),
                        )
                        .child(select),
                )
                .map(|card| {
                    if self.setup_loading() {
                        return card.child(self.status_text());
                    }
                    let Some(entry) = entry else {
                        return card.child(t!("setup.no_compatible_model").into_owned());
                    };
                    if let Some(progress) = models.setup_progress(entry) {
                        return card
                            .child(
                                div().font_semibold().child(
                                    t!(if progress.verifying {
                                        "setup.verifying"
                                    } else {
                                        "setup.downloading"
                                    })
                                    .into_owned(),
                                ),
                            )
                            .child(
                                gpui_kit::component::progress::Progress::new("first-run-progress")
                                    .value(progress.fraction() * 100.0)
                                    .color(rgb(ACTION_ACCENT))
                                    .w_full(),
                            )
                            .child(
                                div()
                                    .text_sm()
                                    .child(crate::models::progress_text(&progress)),
                            )
                            .child(
                                div()
                                    .text_sm()
                                    .text_color(theme.muted_foreground)
                                    .child(t!("setup.can_resume").into_owned()),
                            )
                            .child(
                                Button::new("first-run-cancel")
                                    .self_start()
                                    .label(t!("models.cancel").into_owned())
                                    .on_click(cx.listener(move |this, _, _, cx| {
                                        this.models
                                            .update(cx, |models, cx| models.cancel(entry, cx));
                                    })),
                            );
                    }
                    let free = setup::available_space(models.store().dir()).ok();
                    let fits_disk = |entry: &CatalogEntry| {
                        models.setup_ready(entry)
                            || setup::preflight(models.store(), entry, free).is_ok()
                    };
                    let failure = if models.setup_ready(entry) {
                        None
                    } else {
                        setup::preflight(models.store(), entry, free).err()
                    }
                    .or_else(|| models.setup_failure(entry));
                    let disk_blocked = failure
                        .filter(|failure| matches!(failure, setup::Failure::Disk { .. }))
                        .map(|_| entry);
                    let smaller = failure
                        .filter(|failure| failure.offers_smaller())
                        .and_then(|_| setup::smaller(request, &entries, entry, fits_disk));
                    let choice = smaller.unwrap_or(entry);
                    let show_files_hint = !self.file_tab
                        && setup::recommend(self.setup_request(true), &entries, |_| true)
                            .is_some_and(|files| setup::offers_more_file_formats(choice, files));
                    let ready = models.setup_ready(choice);
                    let consent = choice.is_plamo() && !self.settings.license_accepted;
                    let size = crate::models::format_bytes(choice.bytes);
                    let label = if consent && ready {
                        t!("setup.agree_use")
                    } else if consent {
                        t!("models.agree_download")
                    } else if let Some(failure) = failure {
                        t!(failure.action(smaller.is_some()))
                    } else if ready {
                        t!("models.use")
                    } else if models.store().state(choice).partial().is_some() {
                        t!("setup.resume_download")
                    } else {
                        t!("setup.prepare")
                    };
                    let download = t!(
                        if self.file_tab {
                            "setup.download_files"
                        } else {
                            "setup.download_text"
                        },
                        size = size
                    );
                    let license = if choice.is_plamo() {
                        h_flex()
                            .flex_wrap()
                            .gap_x_1()
                            .child(crate::models::model_license_button(choice))
                    } else {
                        h_flex().child(t!("setup.license", name = "Apache-2.0").into_owned())
                    };
                    card.when_some(failure, |card, failure| {
                        let message = match failure {
                            setup::Failure::Disk { needed } => t!(
                                failure.message(smaller.is_some()),
                                size = crate::models::format_bytes(needed)
                            ),
                            _ => t!(failure.message(smaller.is_some())),
                        };
                        card.child(
                            div()
                                .text_sm()
                                .text_color(theme.danger)
                                .child(message.into_owned()),
                        )
                    })
                    .child(
                        v_flex()
                            .gap_1()
                            .when(smaller.is_some(), |summary| {
                                summary.child(
                                    div()
                                        .font_semibold()
                                        .child(t!("setup.smaller_option").into_owned()),
                                )
                            })
                            .when(!ready, |summary| summary.child(download.into_owned()))
                            .child(license.text_color(theme.muted_foreground))
                            .text_sm(),
                    )
                    .child(
                        v_flex()
                            .gap_1()
                            .text_sm()
                            .text_color(theme.muted_foreground)
                            .when(show_files_hint, |notes| {
                                notes.child(t!("setup.files_hint").into_owned())
                            })
                            .when(failure.is_none() && !ready, |notes| {
                                notes.child(t!("setup.offline").into_owned())
                            }),
                    )
                    .child(
                        files::file_action(
                            Button::new("prepare-model")
                                .self_start()
                                .label(label.into_owned()),
                            false,
                            cx,
                        )
                        .on_click(cx.listener(
                            move |this, _, window, cx| {
                                this.prepare_first_model(choice, disk_blocked, window, cx);
                            },
                        )),
                    )
                })
                .when(!busy, |card| {
                    card.child(
                        Button::new("setup-other-model")
                            .link()
                            .xsmall()
                            .self_start()
                            .label(t!("setup.other_model").into_owned())
                            .on_click(
                                cx.listener(|this, _, window, cx| this.open_models(window, cx)),
                            ),
                    )
                }),
        )
    }

    fn render_download(
        &self,
        entry: &'static CatalogEntry,
        optional_format: Option<engine::profiles::FileFormat>,
        cx: &Context<Self>,
    ) -> impl IntoElement {
        let models = self.models.read(cx);
        let ready = models.setup_ready(entry);
        let consent = entry.is_plamo() && !self.settings.license_accepted;
        let size = crate::models::format_bytes(entry.bytes);
        let format = optional_format.or(self.required_format);
        let optional = optional_format.is_some();
        let label = if consent && ready {
            t!("setup.agree_use").into_owned()
        } else if consent {
            t!("models.agree_download").into_owned()
        } else if ready {
            t!("setup.try").into_owned()
        } else if models.store().state(entry).partial().is_some() {
            t!("setup.resume").into_owned()
        } else {
            t!("models.download").into_owned()
        };
        h_flex()
            .flex_wrap()
            .justify_center()
            .gap_2()
            .when(!(ready && consent), |panel| {
                let message = match format {
                    Some(format) => t!(
                        "setup.required_format",
                        format = files::format_label(format),
                        model = ModelChoice::Catalog(entry).name(),
                        size = &size
                    )
                    .into_owned(),
                    None => t!("setup.required", size = &size).into_owned(),
                };
                let message = if optional {
                    format!("{} · {message}", t!("models.recommended"))
                } else {
                    message
                };
                panel.child(div().text_color(cx.theme().muted_foreground).child(message))
            })
            .when(
                entry.id() == catalog::HY_MT2_7B.id()
                    && crate::models::hy_mt2_7b_memory_warning(self.system_memory(), entry.bytes),
                |panel| {
                    panel.child(
                        div()
                            .text_color(cx.theme().muted_foreground)
                            .child(t!("models.hy_mt2_7b_low_memory").into_owned()),
                    )
                },
            )
            .when_some(models.setup_error(entry), |panel, error| {
                panel.child(error.to_owned())
            })
            .map(|panel| {
                if let Some(progress) = models.setup_progress(entry) {
                    panel
                        .child(
                            gpui_kit::component::progress::Progress::new("setup-progress")
                                .value(progress.fraction() * 100.0)
                                .color(rgb(ACTION_ACCENT))
                                .w(scaled(170.)),
                        )
                        .child(crate::models::progress_text(&progress))
                        .child(
                            Button::new("download-cancel")
                                .label(t!("models.cancel").into_owned())
                                .on_click(cx.listener(move |this, _, _, cx| {
                                    this.models
                                        .update(cx, |models, cx| models.cancel(entry, cx));
                                })),
                        )
                } else {
                    panel
                        .child(
                            // The optional 7B offer must not compete with Translate or Save.
                            if optional {
                                Button::new("download-model").label(label).outline()
                            } else {
                                files::file_action(
                                    Button::new("download-model").label(label),
                                    false,
                                    cx,
                                )
                            }
                            .on_click(cx.listener(
                                move |this, _, window, cx| {
                                    if consent {
                                        this.accept_license(window, cx);
                                    }
                                    this.models.update(cx, |models, cx| {
                                        if optional {
                                            if !models.store().state(entry).is_usable() {
                                                models.start(entry, cx);
                                            }
                                        } else {
                                            models.start_setup(entry, cx);
                                        }
                                    });
                                },
                            )),
                        )
                        .when(consent, |panel| {
                            panel.child(crate::models::model_license_button(entry))
                        })
                }
            })
    }

    fn open_models(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        self.update_recommendation(cx);
        let panel = self.models.clone();
        let backend = self.backend.clone();
        window.open_dialog(cx, move |dialog, _, cx| {
            dialog
                .title(t!("models.title").into_owned())
                .w(scaled_px(900., cx))
                .child(
                    v_flex()
                        .gap_4()
                        .child(panel.clone())
                        .when(panel.read(cx).details(), |body| body.child(backend.clone())),
                )
        });
    }

    fn detected_source_title(&self) -> Option<String> {
        if self.file_tab || !self.languages.automatic {
            return None;
        }
        let request_source = self.shown.as_ref()?.options.source.as_ref();
        let code = shown_detected_source(
            self.languages.source,
            self.detected_text_source.as_ref().map(|source| source.code),
            request_source.map(LangCode::as_str),
        )?;
        Some(t!("source.detected", language = lang::language_name(code)).into_owned())
    }

    fn source_text(&self, cx: &App) -> String {
        self.source.read(cx).value().to_string()
    }

    fn on_source_event(
        &mut self,
        input: &Entity<TextareaState>,
        event: &InputEvent,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if let InputEvent::Change = event {
            self.source_info = None;
            self.source_version += 1;
            let pasted = std::mem::take(&mut self.paste_pending);
            // Drop a replaced source immediately, even if another edit arrives before translation.
            if self.detected_text_source.is_some() {
                lang::discard_detected_source_on_edit(
                    &mut self.detected_text_source,
                    &input.read(cx).value(),
                    pasted,
                );
            }
            if self.auto_translate {
                if pasted {
                    self.translate_now(window, cx);
                } else {
                    self.schedule_translation(window, cx);
                }
            }
            // The character count and the style selector depend on the text.
            cx.notify();
        }
    }

    fn options_changed(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if self.file_tab {
            self.clear_babeldoc_source_notice();
            self.refresh_file_model_requirement(cx);
        } else if self.auto_translate {
            self.translate_now(window, cx);
        }
        cx.notify();
    }

    /// Context or glossary text changed: re-translate after a pause, like typing in the source.
    fn options_edited(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if self.file_tab {
            if self.files.routes_by_context() {
                self.refresh_file_model_requirement(cx);
            }
        } else if self.auto_translate {
            self.schedule_translation(window, cx);
        }
        cx.notify();
    }

    fn schedule_translation(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        self.debounce = cx.spawn_in(window, async move |this, cx| {
            cx.background_executor().timer(DEBOUNCE).await;
            let _ = this.update_in(cx, |this, window, cx| this.translate_now(window, cx));
        });
    }

    fn request_settings(&self, source: SourceChoice) -> RequestSettings {
        RequestSettings {
            profile: self.languages.profile,
            source,
            target: self.languages.target(&self.settings),
            style: self.settings.translation_style,
            document: self.settings.document,
            context: self.context.clone(),
            glossary: self.glossary.clone(),
        }
    }

    fn remember_rollback(&mut self, next: &ModelChoice) {
        self.rollback_model = rollback_before_switch(
            self.rollback_model.take(),
            self.model_ready,
            self.active_model.as_ref(),
            next,
        );
    }

    fn routed_source(
        &self,
        requested: SourceChoice,
        detected: Option<&'static str>,
    ) -> SourceChoice {
        lang::routed_source(self.languages.profile, requested, detected)
    }

    fn pin_model(&mut self, entry: &CatalogEntry, window: &mut Window, cx: &mut Context<Self>) {
        self.settings.model_fixed = true;
        if self.model_ready
            && self
                .active_model
                .as_ref()
                .and_then(ModelChoice::entry)
                .is_some_and(|current| current.id() == entry.id())
        {
            self.settings.model = Some(entry.id().to_owned());
        }
        if self.languages.pin(entry.profile, &mut self.settings) {
            self.model_notice = Some(t!("source.pair_adjusted").into_owned());
        }
        self.required_model = None;
        self.files.clear_model_notice();
        self.settings.save();
        self.refresh_translation_choices(window, cx);
    }

    /// Loads the model chosen by automatic routing, or returns whether it is ready.
    fn use_auto_model(
        &mut self,
        entry: &'static CatalogEntry,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> bool {
        let choice = ModelChoice::Catalog(entry);
        if self.active_model.as_ref() == Some(&choice)
            && (self.model_ready
                || self.pending_loads > 0
                || matches!(self.status, Status::Loading(_) | Status::Measuring { .. }))
        {
            return self.model_ready;
        }
        self.remember_rollback(&choice);
        self.model_notice = None;
        self.load_model(choice, window, cx);
        false
    }

    fn prepare_auto_model(
        &mut self,
        source: Option<&str>,
        target: &str,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> bool {
        if self.required_format.is_none() {
            self.required_model = None;
        }
        let result = self.select_pair(source, Some(target), None, cx);
        if let Some(entry) = result.usable {
            return self.use_auto_model(entry, window, cx);
        }
        let all_failed = self.all_installed_models_failed(cx);
        self.required_model = result
            .suggestion
            .filter(|_| !all_failed && source.is_some())
            .map(|suggestion| suggestion.entry);
        self.required_format = None;
        self.worker.cancel();
        self.current_job = None;
        self.status = if all_failed {
            Status::Error(t!("models.all_loads_failed").into_owned())
        } else if self.required_model.is_some() {
            Status::Ready
        } else if self.show_setup(cx) {
            Status::NoModel
        } else {
            let key = if source == Some(target) {
                "source.change_target"
            } else {
                "source.choose_for_translation"
            };
            Status::Error(t!(key).into_owned())
        };
        cx.notify();
        false
    }

    fn translate_now(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if self.jobs.defer_text() {
            cx.notify();
            return;
        }
        self.debounce = Task::ready(());
        if self.worker_failed {
            return;
        }
        if !self.model_ready && !self.languages.automatic {
            self.translate_on_load = self.active_model.is_some();
            return;
        }
        let text = self.source_text(cx);
        if text.trim().is_empty() {
            if self.required_format.is_none() {
                self.required_model = None;
            }
            self.worker.cancel();
            self.current_job = None;
            self.shown = None;
            self.set_translation(String::new(), window, cx);
            if self.model_ready {
                self.status = Status::Ready;
            }
            cx.notify();
            return;
        }

        let mut source = self.languages.source;
        let source_is_japanese = self.source_info(cx).looks_japanese;
        if self.languages.automatic {
            let code = source.code().or_else(|| {
                lang::retain_detected_source(
                    &mut self.detected_text_source,
                    &text,
                    lang::detect_source(&text),
                )
            });
            let target = self
                .languages
                .target(&self.settings)
                .resolve_for(code.map_or(source_is_japanese, |code| code == "ja"));
            if !self.prepare_auto_model(code, target, window, cx) {
                self.set_translation(String::new(), window, cx);
                self.shown = None;
                self.translate_on_load =
                    matches!(self.status, Status::Loading(_) | Status::Measuring { .. });
                return;
            }
            source = self.routed_source(source, code);
        }
        let request = self.request_settings(source).request(
            text,
            source
                .code()
                .map_or(source_is_japanese, |code| code == "ja"),
        );
        if self.shown.as_ref() == Some(&request) && !self.shown_partial {
            return;
        }
        if let Some(hit) = self.cache.iter().find(|c| c.request == request) {
            let (hit, chunks, cuts) = (
                hit.translation.clone(),
                hit.chunks.clone(),
                hit.cuts.clone(),
            );
            self.worker.cancel();
            self.current_job = None;
            self.shown = Some(request);
            self.shown_version = self.source_version;
            self.set_translation(hit, window, cx);
            self.translated_with = self.active_model.clone();
            self.chunks = chunks;
            self.status = Status::Cached { cut: cuts.len() };
            self.cuts = cuts;
            cx.notify();
            return;
        }

        let target = self.languages.target(&self.settings).resolve_for(
            source
                .code()
                .map_or(source_is_japanese, |code| code == "ja"),
        );
        self.set_translation(String::new(), window, cx);
        // The marker is in the UI language, so it is chosen now rather than kept in the request
        // (where it would make a cached result miss after a language change).
        let document = request
            .document
            .options(appdata::cut_marker(&rust_i18n::locale()).to_owned());
        self.current_job = Some(self.worker.translate(
            request.options.clone(),
            document,
            request.text.clone(),
        ));
        self.stream_tokens = 0;
        self.stream_started = None;
        self.job_started = Some(Instant::now());
        self.current_chunk = None;
        self.shown = Some(request);
        self.shown_version = self.source_version;
        self.translated_with = self.active_model.clone();
        self.status = Status::Translating {
            target,
            chunk: None,
        };
        cx.notify();
    }

    /// Replaces the output and scrolls it back to the top.
    fn set_translation(&mut self, text: String, window: &mut Window, cx: &mut Context<Self>) {
        self.chunks.clear();
        self.alignment = None;
        self.shown_partial = false;
        self.cuts.clear();
        self.next_cut = 0;
        self.translation_utf16 = text.encode_utf16().count();
        self.translation = text;
        let text = SharedString::from(self.translation.clone());
        self.output
            .update(cx, |state, cx| state.set_value(text, window, cx));
    }

    /// Shows newly streamed text, keeping the reader's scroll position and selection.
    fn append_translation(&mut self, piece: &str, window: &mut Window, cx: &mut Context<Self>) {
        let end = self.translation_utf16;
        self.translation.push_str(piece);
        self.translation_utf16 += piece.encode_utf16().count();
        self.edit_output(end..end, piece, window, cx);
    }

    /// Withdraws streamed text after `len` bytes: output that ran away is being retried.
    fn rewind_translation(&mut self, len: usize, window: &mut Window, cx: &mut Context<Self>) {
        if len >= self.translation.len() || !self.translation.is_char_boundary(len) {
            return;
        }
        let removed = self.translation[len..].encode_utf16().count();
        let end = self.translation_utf16;
        self.translation.truncate(len);
        self.translation_utf16 = end - removed;
        self.edit_output(end - removed..end, "", window, cx);
    }

    /// Replaces `range_utf16` of the output pane with `text`.
    ///
    /// Only the changed range is edited. Setting the whole value instead lays out the entire
    /// translation again, which takes longer the longer it grows and eventually stalls the UI.
    fn edit_output(
        &mut self,
        range_utf16: Range<usize>,
        text: &str,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.output.update(cx, |state, cx| {
            let offset = state.scroll_offset();
            let selected = state.selected_range();
            let reversed = state.is_selection_reversed();
            // The pane is read-only for the user; programmatic edits go through the input
            // handler, which honours that flag.
            state.set_readonly(false, cx);
            state.replace_text_in_range(Some(range_utf16), text, window, cx);
            state.set_readonly(true, cx);
            // An edit moves the caret to its end; put the reader's selection back.
            let len = state.text().len();
            let selected = selected.start.min(len)..selected.end.min(len);
            state.set_selected_range_with_direction(selected, reversed, cx);
            state.set_scroll_offset(offset, cx);
        });
    }

    /// The source text's character count and language, computed once per change.
    fn source_info(&mut self, cx: &App) -> SourceInfo {
        *self.source_info.get_or_insert_with(|| {
            let text = self.source.read(cx).value();
            SourceInfo {
                chars: text.chars().count(),
                looks_japanese: lang::looks_japanese(&text),
            }
        })
    }

    /// Stops the running translation, keeping what has been translated so far.
    fn stop_translation(&mut self, cx: &mut Context<Self>) {
        if self.current_job.take().is_some() {
            self.worker.cancel();
            // The partial result must not satisfy a later identical request.
            self.shown_partial = true;
            self.status = Status::Stopped;
            cx.notify();
        }
    }

    /// Handles a batch of worker events, appending consecutive tokens to the output in one go.
    fn on_worker_events(
        &mut self,
        events: Vec<Event>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let mut pending = String::new();
        for event in events {
            match event {
                Event::Token { job, text } => {
                    if self.current_job == Some(job) {
                        pending.push_str(&text);
                        self.stream_tokens += 1;
                        self.stream_started.get_or_insert_with(Instant::now);
                    } else {
                        self.file_token(job, &text);
                    }
                }
                other => {
                    // Later events (Finished in particular) must see the text before them.
                    if !pending.is_empty() {
                        self.append_translation(&std::mem::take(&mut pending), window, cx);
                    }
                    self.on_worker_event(other, window, cx);
                }
            }
        }
        if !pending.is_empty() {
            self.append_translation(&pending, window, cx);
            cx.notify();
        }
        self.install_deferred_model_updates(window, cx);
    }

    fn on_worker_event(&mut self, event: Event, window: &mut Window, cx: &mut Context<Self>) {
        if self.file_event(&event, window, cx) {
            return;
        }
        let is_current = |job| self.current_job == Some(job);
        match event {
            Event::Devices(devices) => {
                // Logged so reports of an unused GPU show whether it was detected at all.
                for d in &devices {
                    crate::app_log!(
                        "device {}: {} ({}, {:?}) {} — {} MiB free of {}",
                        d.index,
                        d.name,
                        d.backend,
                        d.kind,
                        d.description,
                        d.memory_free >> 20,
                        d.memory_total >> 20
                    );
                }
                self.backend.update(cx, |backend, cx| {
                    backend.set_devices(devices.clone(), window, cx)
                });
                self.devices = Some(devices);
                self.update_recommendation(cx);
                if !self.model_ready && self.active_model.is_some() {
                    self.start_load(window, cx);
                }
            }
            // Progress of a load that a newer one already replaces, or of one unloaded since,
            // says nothing.
            Event::LoadProgress(p) if self.pending_loads <= 1 && self.active_model.is_some() => {
                self.status = Status::Loading(p)
            }
            Event::LoadProgress(_) => return,
            Event::Unloaded => {
                crate::app_log!("model released by the worker");
                let (installs, reload) = self.releases.released(self.active_model.as_ref());
                for entry in installs {
                    self.models
                        .update(cx, |models, cx| models.replace(entry, cx));
                }
                // Loaded again whether or not the newer file is in place: the older one stays
                // if it is not.
                if let Some(entry) = reload {
                    self.load_model(ModelChoice::Catalog(entry), window, cx);
                }
                if let Some(entry) = self.pending_delete.take() {
                    let deleted = self.models.update(cx, |models, cx| {
                        models.delete(entry, "the active model, after unloading", cx)
                    });
                    // A file that could not be deleted (another process may still hold it on
                    // Windows) stays the selection; its row shows the error.
                    if deleted && self.settings.model.as_deref() == Some(entry.id()) {
                        self.settings.model = None;
                        self.settings.save();
                    }
                    if deleted && self.show_setup(cx) {
                        self.refresh_target_choices(window, cx);
                    }
                }
            }
            Event::Measuring { index, count } => {
                if let Some(first) = self.measuring {
                    self.status = Status::Measuring {
                        first,
                        step: index + 1,
                        count,
                    };
                }
            }
            Event::Measured(results) => {
                let requested = self.measuring.take() == Some(false);
                self.backend
                    .update(cx, |backend, cx| backend.set_measuring(false, cx));
                let Some(devices) = &self.devices else {
                    return;
                };
                let choice = backend::choose(&results, devices);
                for (result, measured) in results.iter().zip(&choice.results) {
                    match &result.result {
                        Ok(speed) => crate::app_log!(
                            "measured {}: prompt {:.0} tok/s, generation {:.1} tok/s",
                            measured.name,
                            speed.prompt_tokens_per_second,
                            speed.generation_tokens_per_second
                        ),
                        Err(e) => crate::app_log!("measured {}: failed: {e}", measured.name),
                    }
                }
                crate::app_log!(
                    "automatic device: {}",
                    choice.device.as_deref().unwrap_or("CPU")
                );
                self.settings.backend.measured = Some(choice);
                // Measuring from the settings asks for the fastest: automatic selection.
                if requested {
                    self.settings.backend.device = DeviceSetting::Auto;
                }
                self.settings.save();
                let settings = self.settings.backend.clone();
                self.backend
                    .update(cx, |backend, cx| backend.set_settings(settings, window, cx));
                self.update_recommendation(cx);
                self.start_load(window, cx);
            }
            // A load sent before the measurement: the model is released for it, and loaded
            // again after it.
            Event::Loaded(_) | Event::LoadFailed { .. } if self.measuring.is_some() => {
                self.pending_loads = self.pending_loads.saturating_sub(1);
                return;
            }
            Event::Loaded(_) | Event::LoadFailed { .. } if self.pending_loads > 1 => {
                // A newer load follows.
                self.pending_loads -= 1;
                return;
            }
            Event::Loaded(_) => {
                self.pending_loads = 0;
                self.runtime_error = None;
                // Unloaded while loading: the worker releases it next.
                let Some(choice) = self.active_model.clone() else {
                    return;
                };
                let entry = choice.entry();
                self.models
                    .update(cx, |models, cx| models.set_active(entry, cx));
                let first_run = std::mem::take(&mut self.settings.first_run_setup_pending);
                let finished_setup = self.setup_attempt.take().is_some() || first_run;
                let model_changed =
                    entry.is_some_and(|entry| self.settings.model.as_deref() != Some(entry.id()));
                if let Some(entry) = entry.filter(|_| model_changed) {
                    self.settings.model = Some(entry.id().to_owned());
                }
                if first_run || model_changed {
                    self.settings.save();
                }
                if finished_setup && !self.file_tab {
                    self.source.update(cx, |state, cx| state.focus(window, cx));
                }
                self.rollback_model = None;
                let label = choice.label();
                crate::app_log!("model ready: {label}");
                let restored = self.failed_switch.take().map(|model| {
                    t!("status.model_restored", model = model, fallback = label).into_owned()
                });
                if restored.is_some() {
                    self.model_notice = restored.clone();
                }
                self.files.clear_model_notice();
                self.apply_model_profile(choice.profile(), window, cx);
                self.model_ready = true;
                if self
                    .required_model
                    .is_some_and(|required| entry.is_some_and(|entry| required.id() == entry.id()))
                {
                    self.required_model = None;
                }
                self.status = Status::Ready;
                self.refresh_file_model_requirement(cx);
                // With Auto off, an explicitly requested translation still runs.
                let load_for_file = std::mem::take(&mut self.load_for_file);
                if let Some(reason) = restored {
                    self.file_on_load = None;
                    if let Some(id) = self.batch_on_load.take() {
                        self.batch_load_failed(id, &reason, window, cx);
                    }
                    self.status = Status::Ready;
                } else if let Some(id) = self.batch_on_load.take() {
                    self.resume_batch_document(id, window, cx);
                } else if let Some(request) = self.file_on_load.take() {
                    self.submit_file(request, window, cx);
                } else if std::mem::take(&mut self.translate_on_load)
                    || (self.auto_translate && !load_for_file && !self.file_tab)
                {
                    self.translate_now(window, cx);
                }
            }
            Event::LoadFailed { error, kind } => {
                self.pending_loads = 0;
                self.model_load_failed(error, kind, window, cx);
            }
            Event::RuntimeFailed(e) => {
                crate::app_log!("inference runtime failed: {e}");
                let load_failed = runtime_failed_during_load(
                    &self.status,
                    self.pending_loads,
                    self.active_model.is_some(),
                );
                self.runtime_error = Some(e.clone());
                self.worker_failed = true;
                self.pending_loads = 0;
                self.measuring = None;
                self.backend
                    .update(cx, |backend, cx| backend.set_measuring(false, cx));
                self.model_ready = false;
                self.translate_on_load = false;
                self.file_on_load = None;
                self.load_for_file = false;
                self.shown_partial |= self.current_job.take().is_some();
                if load_failed {
                    self.model_load_failed(e, LoadFailure::Other, window, cx);
                } else {
                    self.status = Status::Error(e);
                }
            }
            Event::Chunk {
                job,
                index,
                count,
                source: (start, end),
            } if is_current(job) => {
                if let Status::Translating { chunk, .. } = &mut self.status {
                    *chunk = (count > 1).then_some((index + 1, count));
                }
                self.current_chunk = Some(ChunkMark {
                    start,
                    end,
                    tokens_before: self.stream_tokens,
                });
                self.chunks.push(ChunkSpan {
                    source: start..end,
                    output_start: self.translation.len(),
                });
            }
            Event::Rewind { job, len } if is_current(job) => {
                self.rewind_translation(len, window, cx);
            }
            // Logged without any text, so reports of odd output show whether loops occurred.
            Event::Retry { job, index, reason } if is_current(job) => {
                crate::app_log!(
                    "chunk {}: output ran away ({reason:?}); retrying in smaller pieces",
                    index + 1
                );
                return;
            }
            Event::ParagraphFallback {
                job,
                index,
                paragraphs,
            } if is_current(job) => {
                crate::app_log!(
                    "chunk {}: paragraph layout changed; retrying in {paragraphs} paragraphs",
                    index + 1
                );
                return;
            }
            Event::Cut {
                job,
                index,
                reason,
                source: (start, end),
                output: (output_start, output_end),
            } if is_current(job) => {
                let char_position = self
                    .shown
                    .as_ref()
                    .and_then(|r| r.text.get(..start))
                    .map_or(0, |before| before.chars().count());
                crate::app_log!(
                    "chunk {}: output ran away again ({reason:?}) and was cut; source characters \
                     from {char_position}",
                    index + 1
                );
                self.cuts.push(CutMark {
                    source: start..end,
                    output: output_start..output_end,
                });
                return;
            }
            Event::Finished { job, stats } if is_current(job) => {
                self.current_job = None;
                self.status = match stats.stop_reason {
                    StopReason::Finished => {
                        if let Some(request) = self.shown.clone() {
                            self.remember(Cached {
                                request,
                                translation: self.translation.clone(),
                                chunks: self.chunks.clone(),
                                cuts: self.cuts.clone(),
                            });
                        }
                        Status::Done {
                            tokens: stats.generated_tokens,
                            elapsed: self.job_started.map_or(Duration::ZERO, |t| t.elapsed()),
                            tokens_per_second: stats.generation_tokens_per_second(),
                            cut: stats.cut,
                        }
                    }
                    StopReason::ContextFull => Status::Truncated,
                    // Only superseded jobs are cancelled, and those are no longer current.
                    StopReason::Cancelled => Status::Ready,
                    // Documents resolve runaway output themselves; these end single requests only.
                    StopReason::Repetition { .. }
                    | StopReason::LengthLimit
                    | StopReason::Incomplete => Status::Ready,
                };
            }
            Event::Failed { job, error } if is_current(job) => {
                self.current_job = None;
                self.shown_partial = true;
                self.status = Status::Error(error);
            }
            // Output of superseded jobs.
            Event::Chunk { .. }
            | Event::Token { .. }
            | Event::Segment { .. }
            | Event::Unit { .. }
            | Event::Rewind { .. }
            | Event::Retry { .. }
            | Event::ParagraphFallback { .. }
            | Event::Cut { .. }
            | Event::Finished { .. }
            | Event::Failed { .. } => return,
        }
        cx.notify();
    }

    fn remember(&mut self, entry: Cached) {
        if self.cache.len() == CACHE_SIZE {
            self.cache.pop_front();
        }
        self.cache.push_back(entry);
    }

    fn pane(&self, side: Side) -> &Entity<TextareaState> {
        match side {
            Side::Source => &self.source,
            Side::Output => &self.output,
        }
    }

    /// The alignment of the shown translation with the source, while the source is the text
    /// that was translated.
    fn alignment(&mut self) -> Option<&Alignment> {
        if self.source_version != self.shown_version || self.chunks.is_empty() {
            return None;
        }
        let complete = self.current_job.is_none();
        let key = (self.translation.len(), self.chunks.len(), complete);
        if self.alignment.as_ref().map(|(k, _)| *k) != Some(key) {
            let source = &self.shown.as_ref()?.text;
            let alignment = Alignment::new(source, &self.translation, &self.chunks, complete);
            self.alignment = Some((key, alignment));
        }
        self.alignment.as_ref().map(|(_, a)| a)
    }

    /// A pane notified a change: if the reader scrolled it or moved its caret, the other pane
    /// follows.
    fn on_pane_changed(&mut self, side: Side, window: &mut Window, cx: &mut Context<Self>) {
        let (scroll_y, selection, focused) = {
            let state = self.pane(side).read(cx);
            (
                state.scroll_offset().y,
                state.selected_range(),
                state.focus_handle(cx).is_focused(window),
            )
        };
        let i = side as usize;
        let scrolled = scroll_y != self.sync.scroll_y[i];
        let moved = selection != self.sync.selection[i];
        self.sync.scroll_y[i] = scroll_y;
        self.sync.selection[i] = selection.clone();
        let following = self.sync.followed_at[i].is_some_and(|t| t.elapsed() < FOLLOW_SETTLE);
        if !self.settings.sync_scroll || following {
            return;
        }
        if moved && focused {
            self.follow_caret(side, selection.start, cx);
        } else if scrolled {
            self.follow_scroll(side, cx);
        }
    }

    /// Scrolls the other pane so the counterpart of `side`'s first visible line is at its top.
    fn follow_scroll(&mut self, side: Side, cx: &mut Context<Self>) {
        let Some(top) = top_offset(self.pane(side).read(cx)) else {
            return;
        };
        let other = side.other();
        let Some(target) = self.map_offset(side, top) else {
            return;
        };
        self.scroll_pane(other, target, px(0.), cx);
    }

    /// Selects the counterpart of the paragraph holding `side`'s caret in the other pane, and
    /// scrolls it level with the caret.
    fn follow_caret(&mut self, side: Side, offset: usize, cx: &mut Context<Self>) {
        let other = side.other();
        let Some(range) = self.alignment().and_then(|a| a.counterpart(side, offset)) else {
            return;
        };
        let Some(target) = self.map_offset(side, offset) else {
            return;
        };
        let caret_y = {
            let state = self.pane(side).read(cx);
            match state.line_height() {
                Some(line_height) => {
                    line_height * display_row(state, offset) as f32 + state.scroll_offset().y
                }
                None => px(0.),
            }
        };
        self.pane(other)
            .clone()
            .update(cx, |state, cx| state.set_selected_range(range, cx));
        self.scroll_pane(other, target, caret_y, cx);
    }

    /// The position in the other pane corresponding to `offset` in `side`.
    fn map_offset(&mut self, side: Side, offset: usize) -> Option<usize> {
        self.alignment()?;
        let alignment = &self.alignment.as_ref()?.1;
        let other_text = match side.other() {
            Side::Source => self.shown.as_ref()?.text.as_str(),
            Side::Output => self.translation.as_str(),
        };
        alignment.map(side, offset, other_text)
    }

    /// Scrolls `side` so that `offset` is `y` below the top of the pane.
    fn scroll_pane(&mut self, side: Side, offset: usize, y: Pixels, cx: &mut Context<Self>) {
        let i = side as usize;
        self.sync.followed_at[i] = Some(Instant::now());
        self.sync.applied[i] = Some(cx.spawn(async move |this, cx| {
            cx.background_executor().timer(FOLLOW_APPLIED).await;
            let _ = this.update(cx, |this, cx| {
                this.sync.scroll_y[i] = this.pane(side).read(cx).scroll_offset().y;
            });
        }));
        self.pane(side).clone().update(cx, |state, cx| {
            let Some(line_height) = state.line_height() else {
                return;
            };
            let row = display_row(state, offset.min(state.text().len()));
            let mut scroll = state.scroll_offset();
            scroll.y = y - line_height * row as f32;
            if scroll.y > px(0.) {
                scroll.y = px(0.);
            }
            state.set_scroll_offset(scroll, cx);
        });
    }

    /// Selects the next passage that was cut in both panes and scrolls to it.
    fn show_next_cut(&mut self, cx: &mut Context<Self>) {
        let Some(cut) = self
            .cuts
            .get(self.next_cut % self.cuts.len().max(1))
            .cloned()
        else {
            return;
        };
        self.next_cut += 1;
        // A few lines below the top, so the text before it is in view too.
        let y = scaled_px(48., cx);
        self.scroll_pane(Side::Output, cut.output.start, y, cx);
        self.output
            .update(cx, |state, cx| state.set_selected_range(cut.output, cx));
        if self.source_version == self.shown_version {
            self.scroll_pane(Side::Source, cut.source.start, y, cx);
            self.source
                .update(cx, |state, cx| state.set_selected_range(cut.source, cx));
        }
    }

    /// Checks the local installation at startup, then checks for releases if enabled.
    fn check_for_update(&mut self, trigger: UpdateCheckTrigger, cx: &mut Context<Self>) {
        if_updates_enabled(RELEASE_UPDATE_CHECK_ENABLED, || {
            self.run_update_check(trigger, cx)
        });
    }

    fn run_update_check(&mut self, trigger: UpdateCheckTrigger, cx: &mut Context<Self>) {
        if self.update_check.checking_key().is_some() {
            return;
        }
        self.update_check = UpdateCheck::Checking;
        cx.notify();

        let current = Version::current();
        self._update_task = cx.spawn(async move |this, cx| {
            if trigger.probes_installation() {
                // Signature verification walks the entire bundle on macOS.
                let (installation, failed, prepared) = cx
                    .background_spawn(async move {
                        // The resident app can reopen its window without starting a new process.
                        static INSTALLATION: std::sync::OnceLock<
                            Result<Installation, UpdateError>,
                        > = std::sync::OnceLock::new();
                        let mut failed = None;
                        let installation = INSTALLATION.get_or_init(|| {
                            let installation = Installation::current()?;
                            failed = installation.clean_up(current);
                            Ok(installation)
                        });
                        let prepared = installation
                            .as_ref()
                            .ok()
                            .and_then(|installation| installation.prepared(current));
                        (installation.clone(), failed, prepared)
                    })
                    .await;
                let _ = this.update(cx, |this, cx| {
                    this.installation = Some(installation);
                    if let Some(error) = failed {
                        crate::app_log!("the last update failed: {error}");
                        this.install = InstallState::Failed(error);
                    } else if let Some(version) = prepared {
                        this.install = InstallState::Ready(version);
                    }
                    cx.notify();
                });
            }
            let can_check = this.update(cx, |this, cx| {
                let enabled = trigger.can_check_network(this.settings.check_updates);
                if !enabled {
                    this.update_check = UpdateCheck::Idle;
                    cx.notify();
                }
                enabled
            });
            if !can_check.unwrap_or(false) {
                return;
            }

            let manual = matches!(trigger, UpdateCheckTrigger::Manual);
            let (request_tx, request) = async_channel::bounded(1);
            let job = cx.background_spawn(async move {
                let path = settings::update_check_path();
                let mut cache = CheckCache::load(&path);
                let result = cache.check(&path, manual, move || {
                    let _ = request_tx.try_send(());
                });
                (cache, result)
            });
            if request.recv().await.is_ok() {
                let _ = this.update(cx, |this, cx| {
                    this.update_check = UpdateCheck::Requesting;
                    cx.notify();
                });
            }
            let (cache, result) = job.await;
            let _ = this.update(cx, |this, cx| {
                let show_release = manual || this.settings.check_updates;
                this.update_check = match result {
                    Ok(Some(release)) => {
                        let version = release.version;
                        crate::app_log!(
                            "update check: the newest release is {version}, this build {current}"
                        );
                        let newer = version > current;
                        if show_release {
                            this.newer_release = newer.then_some(release);
                            if matches!(this.install, InstallState::Ready(ready) if ready < version)
                            {
                                this.install = InstallState::Idle;
                            }
                        }
                        if newer {
                            UpdateCheck::Found(version)
                        } else {
                            UpdateCheck::UpToDate(version)
                        }
                    }
                    Ok(None) => UpdateCheck::Idle,
                    Err(e) => {
                        if show_release {
                            this.newer_release = cache.newer_than(current).cloned();
                        }
                        crate::app_log!("update check failed: {e}");
                        UpdateCheck::Failed(e.to_string())
                    }
                };
                cx.notify();
            });
        });
    }

    /// Downloads the newest release and unpacks it next to the app.
    fn install_update(&mut self, cx: &mut Context<Self>) {
        let (Some(Ok(installation)), Some(release)) =
            (self.installation.as_ref(), self.newer_release.as_ref())
        else {
            return;
        };
        if matches!(
            self.install,
            InstallState::Downloading { .. } | InstallState::Verifying(_)
        ) {
            return;
        }
        let installation = installation.clone();
        self.install = InstallState::Downloading {
            version: release.version,
            done: 0,
            total: 0,
        };
        cx.notify();
        self._install_task = cx.spawn(async move |this, cx| {
            let (progress_tx, progress) = async_channel::unbounded();
            let job = cx.background_spawn(async move {
                // The check's answer may be a day old: the files are taken as they are now.
                let release = update::latest()?;
                let current = Version::current();
                if release.version <= current {
                    return Err(update::UpdateError::Format(format!(
                        "the newest release, {}, is not newer than {current}",
                        release.version
                    )));
                }
                let version = release.version;
                crate::app_log!(
                    "downloading {} for the update to {version}",
                    installation.asset_name(version)
                );
                let mut last: Option<Instant> = None;
                installation.prepare(&release, &mut |done, total| {
                    // Ten times a second is enough for the status bar.
                    if done == total
                        || last.is_none_or(|t| t.elapsed() >= Duration::from_millis(100))
                    {
                        last = Some(Instant::now());
                        let _ = progress_tx.try_send((version, done, total));
                    }
                })?;
                Ok(version)
            });
            // Ends when the job does: it holds the sender.
            while let Ok((version, done, total)) = progress.recv().await {
                let _ = this.update(cx, |this, cx| {
                    if let InstallState::Downloading { .. } = this.install {
                        this.install = InstallState::Downloading {
                            version,
                            done,
                            total,
                        };
                        cx.notify();
                    }
                });
            }
            let result = job.await;
            let _ = this.update(cx, |this, cx| {
                this.install = match result {
                    Ok(version) => {
                        crate::app_log!("the update to {version} is ready to install");
                        InstallState::Ready(version)
                    }
                    Err(e) => {
                        crate::app_log!("the update failed: {e}");
                        InstallState::Failed(e.to_string())
                    }
                };
                cx.notify();
            });
        });
    }

    /// The newer release: what installing it does, its notes, and the button that starts it.
    fn open_update(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let Some(release) = self.newer_release.clone() else {
            return;
        };
        let installable = matches!(self.installation, Some(Ok(_)));
        let cannot_replace = matches!(self.installation, Some(Err(UpdateError::InstallLocation)));
        let failure = match &self.install {
            InstallState::Failed(error) => Some(error.clone()),
            _ => None,
        };
        let view = cx.entity().downgrade();
        window.open_dialog(cx, move |dialog, _, cx| {
            let muted = cx.theme().muted_foreground;
            let page = release.page.clone();
            let notes_page = page.clone();
            let install_view = view.clone();
            let primary = if installable {
                files::file_action(Button::new("update-install"), false, cx)
                    .label(t!("update.download").into_owned())
                    .on_click(move |_, window, cx| {
                        window.close_dialog(cx);
                        let _ = install_view.update(cx, |this, cx| this.install_update(cx));
                    })
            } else {
                files::file_action(Button::new("update-page"), false, cx)
                    .label(t!("update.open_page").into_owned())
                    .on_click(move |_, window, cx| {
                        window.close_dialog(cx);
                        cx.open_url(&page);
                    })
            };
            dialog
                .title(t!("update.title", version = release.version.to_string()).into_owned())
                .w(scaled_px(520., cx))
                .child(
                    v_flex()
                        .gap_3()
                        .text_sm()
                        .child(if installable {
                            t!("update.body", version = release.version.to_string()).into_owned()
                        } else if cannot_replace {
                            t!("update.location").into_owned()
                        } else {
                            t!("update.manual").into_owned()
                        })
                        .children(failure.clone().map(|error| {
                            div()
                                .text_xs()
                                .text_color(muted)
                                .child(t!("update.last_failed", error = error).into_owned())
                        }))
                        .when(installable, |body| {
                            body.child(
                                Button::new("update-notes")
                                    .link()
                                    .label(t!("update.notes").into_owned())
                                    .on_click(move |_, _, cx| cx.open_url(&notes_page)),
                            )
                        }),
                )
                .footer(
                    h_flex()
                        .w_full()
                        .justify_end()
                        .gap_2()
                        .child(
                            Button::new("update-later")
                                .label(t!("update.later").into_owned())
                                .on_click(|_, window, cx| window.close_dialog(cx)),
                        )
                        .child(primary),
                )
        });
    }

    fn set_check_updates(&mut self, enabled: bool, cx: &mut Context<Self>) {
        if self.settings.check_updates == enabled {
            return;
        }
        self.settings.check_updates = enabled;
        self.settings.save();
        crate::app_log!("update checks {}", if enabled { "on" } else { "off" });
        if enabled {
            self.check_for_update(UpdateCheckTrigger::Automatic, cx);
        } else {
            self.newer_release = None;
            cx.notify();
        }
    }

    fn set_sync_scroll(&mut self, enabled: bool, cx: &mut Context<Self>) {
        if self.settings.sync_scroll == enabled {
            return;
        }
        self.settings.sync_scroll = enabled;
        self.settings.save();
        crate::app_log!(
            "synchronized scrolling {}",
            if enabled { "on" } else { "off" }
        );
        self.link_selection_focus(cx);
        cx.notify();
    }

    fn set_double_copy(&mut self, enabled: bool, cx: &mut Context<Self>) {
        if self.settings.double_copy == enabled {
            return;
        }
        self.settings.double_copy = enabled;
        self.settings.save();
        crate::app_log!("double copy {}", if enabled { "on" } else { "off" });
        crate::shell::set_double_copy(enabled, cx);
        cx.notify();
    }

    fn set_resident(&mut self, enabled: bool, cx: &mut Context<Self>) {
        if self.settings.resident == enabled {
            return;
        }
        self.settings.resident = enabled;
        self.settings.save();
        crate::app_log!("staying resident {}", if enabled { "on" } else { "off" });
        crate::shell::set_resident(enabled, cx);
        cx.notify();
    }

    /// Puts `text` in the source and translates it, with Auto off too: a double copy in another
    /// application asks for it.
    pub fn translate_text(&mut self, text: String, window: &mut Window, cx: &mut Context<Self>) {
        self.detected_text_source = None;
        self.source.update(cx, |state, cx| {
            state.set_value(SharedString::from(text), window, cx)
        });
        self.source_info = None;
        self.source_version += 1;
        self.translate_now(window, cx);
        cx.notify();
    }

    /// While scrolling is synchronized, each pane keeps its selection (the counterpart of the
    /// other pane's caret) visible while the other pane has focus.
    fn link_selection_focus(&self, cx: &mut Context<Self>) {
        let enabled = self.settings.sync_scroll;
        let source_focus = self.source.read(cx).focus_handle(cx);
        let output_focus = self.output.read(cx).focus_handle(cx);
        self.source.update(cx, |state, cx| {
            state.set_selection_focus(enabled.then_some(output_focus), cx)
        });
        self.output.update(cx, |state, cx| {
            state.set_selection_focus(enabled.then_some(source_focus), cx)
        });
    }

    fn can_swap(&self) -> bool {
        // While translating, the output is still partial.
        self.current_job.is_none() && self.languages.can_swap(&self.settings)
    }

    /// Swaps the languages and moves the translation into the source. With Japanese ⇄ English
    /// as the target, an explicit source becomes the translation's language (the target stays).
    fn swap(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        self.detected_text_source = None;
        if self.languages.swap(&mut self.settings) {
            self.settings.save();
            let source = self.languages.source;
            self.source_select.update(cx, |state, cx| {
                state.set_selected_value(&source, window, cx)
            });
            self.refresh_target_choices(window, cx);
        }
        if !self.translation.is_empty() {
            let text = SharedString::from(self.translation.clone());
            self.source
                .update(cx, |state, cx| state.set_value(text, window, cx));
            self.source_info = None;
            self.source_version += 1;
        }
        self.translate_now(window, cx);
        cx.notify();
    }

    /// Ctrl+Enter (Cmd+Enter) in any of the text inputs: see `bind_keys`.
    fn translate_now_action(
        &mut self,
        _: &TranslateNow,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if self.file_tab {
            self.start_file(window, cx);
        } else {
            self.translate_now(window, cx);
        }
    }

    fn copy_translation(
        &mut self,
        _: &CopyTranslation,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if self.translation.is_empty() {
            return;
        }
        cx.write_to_clipboard(ClipboardItem::new_string(self.translation.clone()));
        self.copied = true;
        self.copied_reset = cx.spawn_in(window, async move |this, cx| {
            cx.background_executor().timer(COPIED_FEEDBACK).await;
            let _ = this.update(cx, |this, cx| {
                this.copied = false;
                cx.notify();
            });
        });
        cx.notify();
    }

    fn clear(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        self.detected_text_source = None;
        self.source.update(cx, |state, cx| {
            state.set_value("", window, cx);
            state.focus(window, cx);
        });
        self.source_info = None;
        self.source_version += 1;
        self.translate_now(window, cx);
    }

    /// Estimated time until the running job finishes, once a chunk of a long text is done.
    ///
    /// Progress is measured in source bytes. The chunk in progress counts in proportion to its
    /// output so far, against what the finished chunks produced per source byte.
    fn remaining_time(&self) -> Option<Duration> {
        let chunk = self.current_chunk?;
        let total = self.shown.as_ref()?.text.len();
        let elapsed = self.job_started?.elapsed().as_secs_f64();
        // Nothing to go by before a chunk has been translated.
        if chunk.tokens_before == 0 || chunk.start == 0 || total == 0 || elapsed < 1.0 {
            return None;
        }
        let tokens_per_byte = chunk.tokens_before as f64 / chunk.start as f64;
        let chunk_len = (chunk.end - chunk.start) as f64;
        let chunk_tokens = (self.stream_tokens - chunk.tokens_before) as f64;
        // Never count the chunk as finished before it is.
        let chunk_done = (chunk_tokens / (chunk_len * tokens_per_byte).max(1.0)).min(0.95);
        let done = chunk.start as f64 + chunk_len * chunk_done;
        let remaining = (total as f64 - done).max(0.0) * elapsed / done;
        Some(Duration::from_secs_f64(remaining))
    }

    /// Generation rate of the running job, measured from the first received token.
    /// It includes prompt processing between chunks of a long text.
    fn live_tokens_per_second(&self) -> Option<f64> {
        let elapsed = self.stream_started?.elapsed().as_secs_f64();
        (elapsed >= 0.3 && self.stream_tokens > 1)
            .then(|| (self.stream_tokens - 1) as f64 / elapsed)
    }

    fn render_model_menu(&self, cx: &Context<Self>) -> impl IntoElement {
        let mut entries: Vec<_> = catalog::entries()
            .into_iter()
            .filter(|entry| self.models.read(cx).store().state(entry).is_usable())
            .collect();
        entries.sort_by_cached_key(|entry| {
            (
                entry.is_plamo(),
                entry.base_model(),
                self.active_model.as_ref().and_then(ModelChoice::entry) != Some(*entry),
            )
        });
        entries.dedup_by(|right, left| right.base_model() == left.base_model());
        let entries = entries.into_iter();
        let needs_consent =
            !self.settings.license_accepted && entries.clone().any(|entry| entry.is_plamo());
        let automatic = Button::new("model-auto")
            .label(format!(
                "{} {}",
                if self.languages.automatic { "✓" } else { " " },
                t!("models.automatic_recommended")
            ))
            .ghost()
            .small()
            .on_click(cx.listener(|this, _, window, cx| {
                this.model_menu = false;
                this.required_model = None;
                this.settings.target_language = this.languages.target(&this.settings);
                this.settings.model_fixed = false;
                this.languages.automatic = true;
                this.settings.save();
                this.refresh_translation_choices(window, cx);
                this.refresh_file_model_requirement(cx);
                cx.notify();
            }));
        h_flex().justify_end().px_3().child(
            v_flex()
                .gap_1()
                .child(automatic.self_start())
                .when(needs_consent, |menu| {
                    menu.child(crate::models::plamo_license_button().self_start())
                })
                .children(entries.map(|entry| {
                    Button::new(SharedString::from(entry.id()))
                        .self_start()
                        .ghost()
                        .small()
                        .label(format!(
                            "{} {}",
                            if !self.languages.automatic
                                && self.settings.model.as_deref() == Some(entry.id())
                            {
                                "✓"
                            } else {
                                " "
                            },
                            ModelChoice::Catalog(entry).name()
                        ))
                        .when(
                            entry.is_plamo() && !self.settings.license_accepted,
                            |button| {
                                button.label(format!(
                                    "{} · {}",
                                    t!("setup.agree_use"),
                                    ModelChoice::Catalog(entry).label()
                                ))
                            },
                        )
                        .on_click(cx.listener(move |this, _, window, cx| {
                            if entry.is_plamo() && !this.settings.license_accepted {
                                this.accept_license(window, cx);
                            }
                            this.model_menu = false;
                            this.required_model = None;
                            this.on_models_event(
                                &this.models.clone(),
                                &ModelsEvent::Use(entry),
                                window,
                                cx,
                            );
                        }))
                }))
                .child(div().border_t_1().border_color(cx.theme().border))
                .child(
                    div()
                        .px_2()
                        .text_xs()
                        .text_color(cx.theme().muted_foreground)
                        .child(t!("models.quality").into_owned()),
                )
                .child(h_flex().gap_1().children([false, true].map(|high| {
                    Button::new(if high {
                        "quality-high"
                    } else {
                        "quality-standard"
                    })
                    .label(
                        if high {
                            t!("setup.high")
                        } else {
                            t!("setup.standard")
                        }
                        .into_owned(),
                    )
                    .small()
                    .when(self.settings.model_high_quality == high, |button| {
                        files::file_action(button, false, cx)
                    })
                    .on_click(cx.listener(move |this, _, _, cx| {
                        this.settings.model_high_quality = high;
                        this.settings.save();
                        this.model_menu = false;
                        cx.notify();
                    }))
                })))
                .child(div().border_t_1().border_color(cx.theme().border))
                .child(
                    Button::new("manage-models")
                        .self_start()
                        .ghost()
                        .small()
                        .label(t!("models.manage").into_owned())
                        .on_click(cx.listener(|this, _, window, cx| {
                            this.model_menu = false;
                            this.open_models(window, cx);
                        })),
                ),
        )
    }

    fn status_text(&self) -> String {
        if (self.file_tab || self.jobs.busy())
            && self.batch.visible()
            && !matches!(self.status, Status::Loading(_) | Status::Error(_))
        {
            return self.batch.summary();
        }
        match &self.status {
            Status::NoModel => t!("status.no_model").into_owned(),
            Status::Unloaded => t!("status.unloaded").into_owned(),
            Status::LicenseRequired => t!("status.license_required").into_owned(),
            Status::Loading(_)
                if self.rollback_model.is_some()
                    || self.batch_on_load.is_some()
                    || self.file_on_load.is_some() =>
            {
                let model = self
                    .active_model
                    .as_ref()
                    .map_or_else(String::new, ModelChoice::name);
                t!("status.switching_model", model = model).into_owned()
            }
            Status::Loading(p) => {
                t!("status.loading", percent = format!("{:.0}", p * 100.0)).into_owned()
            }
            Status::Measuring { first, step, count } => {
                let key = if *first {
                    "status.measuring_first"
                } else {
                    "status.measuring"
                };
                t!(key, step = step.max(&1), count = count.max(&1)).into_owned()
            }
            Status::Ready => t!("status.ready").into_owned(),
            Status::Translating { target, chunk } => {
                let mut text =
                    t!("status.translating", lang = lang::language_name(target)).into_owned();
                if let Some((current, total)) = chunk {
                    text.push_str(&format!(" ({current}/{total})"));
                }
                if self.stream_tokens > 0 {
                    let tokens = t!(
                        "status.tokens",
                        count = lang::format_count(self.stream_tokens)
                    );
                    text.push_str(&format!(" · {tokens}"));
                }
                if let Some(rate) = self.live_tokens_per_second() {
                    let speed = lang::format_decimal(format!("{rate:.1}"));
                    text.push_str(&format!(" · {}", t!("status.speed", speed = speed)));
                }
                if let Some(eta) = self.remaining_time() {
                    let eta = t!("status.remaining", time = lang::format_duration(eta));
                    text.push_str(&format!(" · {eta}"));
                }
                text
            }
            Status::Done {
                tokens,
                elapsed,
                tokens_per_second,
                cut,
            } => {
                let speed = lang::format_decimal(format!("{tokens_per_second:.1}"));
                let tokens = lang::format_count(*tokens);
                let time = lang::format_duration(*elapsed);
                if *cut > 0 {
                    t!(
                        "status.done_cut",
                        tokens = tokens,
                        speed = speed,
                        time = time,
                        count = cut
                    )
                    .into_owned()
                } else {
                    t!("status.done", tokens = tokens, speed = speed, time = time).into_owned()
                }
            }
            Status::Cached { cut: 0 } => t!("status.cached").into_owned(),
            Status::Cached { cut } => t!("status.cached_cut", count = cut).into_owned(),
            Status::Stopped => t!("status.stopped").into_owned(),
            Status::Truncated => t!("status.truncated").into_owned(),
            Status::Error(message) => t!("status.error", message = message).into_owned(),
        }
    }
}

impl Render for TranslatorView {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let source = self.source_info(cx);
        let char_count = source.chars;
        let source_is_japanese = if self.file_tab {
            self.file_source_is_japanese()
                .unwrap_or(source.looks_japanese)
        } else {
            source.looks_japanese
        };
        let missing_models_folder = self.models_folder_missing();
        self.models_folder_was_missing |= missing_models_folder;
        let show_setup = self.show_setup(cx);
        let setup_busy = self.setup_busy(cx);
        let batch_tab = self.file_tab && self.batch.visible();
        let fixed = self.file_tab.then(|| self.batch.fixed_settings()).flatten();
        let target = fixed.map_or(self.languages.target(&self.settings), |s| s.target);
        let profile = self.languages.profile;
        let shows_style = profile.capabilities().japanese_style
            && target.resolve_for(if batch_tab { false } else { source_is_japanese }) == "ja";
        let term_count = fixed.map_or(self.glossary.len(), |s| s.glossary.len());
        let view = cx.entity().downgrade();
        let theme = cx.theme();
        let muted = theme.muted_foreground;

        let model_button_label = self.active_model.as_ref().map_or_else(
            || {
                t!(if self.languages.automatic {
                    "models.automatic"
                } else {
                    "models.choose"
                })
                .into_owned()
            },
            |model| {
                if self.languages.automatic {
                    format!("{} · {}", t!("models.automatic"), model.name())
                } else {
                    model.name()
                }
            },
        );
        // Only the title lives in the title bar: on Linux its drag-to-move handling can swallow
        // clicks on controls placed there, so buttons go in the toolbar and status bar.
        let title_bar = TitleBar::new().child(
            div()
                .text_sm()
                .font_semibold()
                .child(t!("app.title").into_owned()),
        );
        let model_button = Button::new("models")
            .disabled(keeps_model(&self.jobs))
            .xsmall()
            .label(model_button_label);
        let model_menu = gpui_kit::component::popover::Popover::new("model-menu")
            .anchor(Anchor::BottomRight)
            .open(self.model_menu && !keeps_model(&self.jobs))
            .on_open_change(cx.listener(|this, open, _, cx| {
                this.model_menu = *open;
                cx.notify();
            }))
            .trigger(model_button)
            .when(self.model_menu && !keeps_model(&self.jobs), |menu| {
                menu.child(self.render_model_menu(cx))
            });
        let settings_button = Button::new("settings")
            .disabled(self.jobs.busy())
            .ghost()
            .xsmall()
            .icon(IconName::Settings)
            .tooltip(t!("settings.title").into_owned())
            .on_click(cx.listener(|this, _, window, cx| this.open_settings(window, cx)));
        let update_button = match (&self.install, &self.newer_release) {
            (InstallState::Ready(version), _) => Some(
                Button::new("update")
                    .xsmall()
                    .icon(Lucide::CircleArrowUp)
                    .label(t!("update.restart", version = version.to_string()).into_owned())
                    .on_click(
                        cx.listener(|this, _, window, cx| this.request_update_restart(window, cx)),
                    ),
            ),
            (InstallState::Verifying(version), _) => Some(
                Button::new("update")
                    .xsmall()
                    .loading(true)
                    .label(t!("update.restart", version = version.to_string()).into_owned()),
            ),
            (
                InstallState::Downloading {
                    version,
                    done,
                    total,
                },
                _,
            ) => {
                let percent = (*done * 100).checked_div(*total).unwrap_or(0);
                Some(
                    Button::new("update").ghost().xsmall().loading(true).label(
                        t!(
                            "update.downloading",
                            version = version.to_string(),
                            percent = percent.to_string()
                        )
                        .into_owned(),
                    ),
                )
            }
            (state, Some(release)) => {
                let version = release.version.to_string();
                let label = if matches!(state, InstallState::Failed(_)) {
                    t!("update.failed", version = version)
                } else {
                    t!("update.available", version = version)
                };
                Some(
                    Button::new("update")
                        .ghost()
                        .xsmall()
                        .icon(Lucide::CircleArrowUp)
                        .label(label.into_owned())
                        .on_click(cx.listener(|this, _, window, cx| this.open_update(window, cx))),
                )
            }
            (_, None) => None,
        };

        let file_toolbar_width = scaled_px(files::FILE_PANEL_WIDTH + 40., cx);
        let split_file_toolbar = self.file_tab
            && window.viewport_size().width.min(file_toolbar_width)
                < scaled_px(if shows_style { 800. } else { 690. }, cx);
        let shrink_language_slots = !self.file_tab || split_file_toolbar;
        let source_width = scaled(if self.file_tab { 190. } else { 210. });
        let target_width = scaled(if self.file_tab { 215. } else { 230. });
        let language_controls = h_flex()
            .gap_2()
            .when(split_file_toolbar, |group| group.w_full().min_w_0())
            .when(!self.file_tab, |group| {
                group.flex_none().max_w_full().min_w_0()
            })
            .map(|toolbar| {
                // The live source menu can supply BabelDOC's source before a batch starts.
                // While it runs, show the source policy captured for the other files.
                let batch_source = if !batch_tab {
                    None
                } else if let Some(settings) = fixed {
                    Some(settings.source)
                } else if profile.capabilities().directions.is_none()
                    && !self.batch_source_for_babeldoc()
                {
                    Some(SourceChoice::Detect)
                } else {
                    None
                };
                if let Some(source) = batch_source {
                    toolbar.child(div().w(scaled(190.)).child(
                        source.code().map_or_else(
                            || t!("files.detect_each").into_owned(),
                            lang::language_name,
                        ),
                    ))
                } else {
                    toolbar.child(
                        div()
                            .w(source_width)
                            .when(shrink_language_slots, |slot| slot.flex_initial().min_w_0())
                            .child(
                                Select::new(&self.source_select)
                                    .w_full()
                                    .menu_width(scaled(260.))
                                    .when_some(self.detected_source_title(), |select, label| {
                                        select.accessibility_label(label)
                                    })
                                    .search_placeholder(t!("search.language").into_owned()),
                            ),
                    )
                }
            })
            .map(|toolbar| {
                if self.file_tab {
                    toolbar
                        .child(gpui_kit::component::Icon::new(Lucide::ArrowRight).text_color(muted))
                } else {
                    toolbar.child(
                        Button::new("swap")
                            .ghost()
                            .icon(Lucide::ArrowLeftRight)
                            .tooltip(t!("action.swap").into_owned())
                            .disabled(!self.can_swap())
                            .on_click(cx.listener(|this, _, window, cx| this.swap(window, cx))),
                    )
                }
            })
            .map(|bar| {
                if let Some(settings) = fixed {
                    let target = match settings.target {
                        TargetChoice::JapaneseEnglish => t!("target.ja_en").into_owned(),
                        TargetChoice::Lang(code) => lang::language_name(code),
                    };
                    let style = match settings.style {
                        StyleChoice::Default => "style.default",
                        StyleChoice::Plain => "style.plain",
                        StyleChoice::Polite => "style.polite",
                    };
                    bar.child(div().child(target)).when(shows_style, |bar| {
                        bar.child(div().child(t!(style).into_owned()))
                    })
                } else {
                    bar.child(
                        div()
                            .w(target_width)
                            .when(shrink_language_slots, |slot| slot.flex_initial().min_w_0())
                            .child(
                                Select::new(&self.target_select)
                                    .w_full()
                                    .menu_width(scaled(260.))
                                    .search_placeholder(t!("search.language").into_owned()),
                            ),
                    )
                    .when(shows_style, |bar| {
                        bar.child(
                            div()
                                .w(scaled(if self.file_tab { 155. } else { 190. }))
                                .when(shrink_language_slots, |slot| slot.flex_initial().min_w_0())
                                .child(Select::new(&self.style_select).w_full()),
                        )
                    })
                }
            });

        let actions = h_flex()
            .when(!self.file_tab, |group| {
                group.flex_none().max_w_full().flex_wrap().justify_end()
            })
            .ml_auto()
            .gap_2()
            .child(
                Button::new("options")
                    .ghost()
                    .icon(IconName::Settings2)
                    .label(if term_count == 0 {
                        t!("options.toggle").into_owned()
                    } else {
                        t!("options.toggle_with_terms", count = term_count).into_owned()
                    })
                    .selected(self.show_options)
                    .on_click(cx.listener(|this, _, _, cx| {
                        this.show_options = !this.show_options;
                        cx.notify();
                    })),
            )
            .when(!self.file_tab && !show_setup, |toolbar| {
                toolbar.child(
                    Switch::new("auto-translate")
                        .color(rgb(ACTION_ACCENT))
                        .checked(self.auto_translate)
                        .label(t!("action.auto_translate").into_owned())
                        .on_click(cx.listener(|this, checked: &bool, window, cx| {
                            this.auto_translate = *checked;
                            if this.auto_translate {
                                this.translate_now(window, cx);
                            } else {
                                this.debounce = Task::ready(());
                            }
                            cx.notify();
                        })),
                )
            })
            .when(
                !self.file_tab
                    && !show_setup
                    && self
                        .required_model
                        .filter(|_| self.required_format.is_none())
                        .is_none(),
                |toolbar| {
                    toolbar.child(if self.current_job.is_some() {
                        Button::new("stop")
                            .danger()
                            .label(t!("action.stop").into_owned())
                            .on_click(cx.listener(|this, _, _, cx| this.stop_translation(cx)))
                    } else {
                        files::file_action(
                            Button::new("translate"),
                            !self.model_ready && !self.languages.automatic,
                            cx,
                        )
                        .label(t!("action.translate").into_owned())
                        .disabled(!self.model_ready && !self.languages.automatic)
                        .on_click(cx.listener(|this, _, window, cx| {
                            if this.file_tab {
                                this.start_file(window, cx);
                            } else {
                                this.translate_now(window, cx);
                            }
                        }))
                    })
                },
            );

        let toolbar_row = || h_flex().w_full().gap_2();
        let toolbar = if self.file_tab {
            v_flex()
                .w_full()
                .max_w(file_toolbar_width)
                .mx_auto()
                .px_5()
                .py_2()
                .gap_2()
                .flex_none()
                .map(|toolbar| {
                    if split_file_toolbar {
                        toolbar.child(language_controls).child(actions)
                    } else {
                        toolbar.child(toolbar_row().child(language_controls).child(actions))
                    }
                })
        } else {
            // Wrap the two groups as a unit when the translated labels fill the row.
            toolbar_row()
                .flex_none()
                .flex_wrap()
                .px_3()
                .py_2()
                .child(language_controls)
                .child(actions)
        };

        let source_pane = v_flex()
            .flex_1()
            .min_w_0()
            .h_full()
            .gap_1()
            .child(
                Textarea::new(&self.source)
                    .flex_1()
                    .h_full()
                    .on_paste(move |_, _, cx| {
                        let _ = view.update(cx, |this, _| this.paste_pending = true);
                        // Let the textarea insert the text as usual.
                        false
                    }),
            )
            .child(
                h_flex()
                    .justify_between()
                    .text_xs()
                    .text_color(theme.muted_foreground)
                    .child(t!("source.chars", count = char_count).into_owned())
                    .child(
                        Button::new("clear")
                            .ghost()
                            .xsmall()
                            .icon(IconName::Close)
                            .label(t!("action.clear").into_owned())
                            .disabled(source.chars == 0)
                            .on_click(cx.listener(|this, _, window, cx| this.clear(window, cx))),
                    ),
            );

        let output_pane = v_flex()
            .flex_1()
            .min_w_0()
            .h_full()
            .gap_1()
            .map(|pane| {
                if let Some(entry) = self
                    .required_model
                    .filter(|_| self.required_format.is_none())
                {
                    pane.child(self.render_download(entry, None, cx))
                } else {
                    pane.child(Textarea::new(&self.output).readonly(true).flex_1().h_full())
                }
            })
            .child(
                h_flex().justify_end().child(
                    Button::new("copy")
                        .ghost()
                        .xsmall()
                        .icon(if self.copied {
                            IconName::Check
                        } else {
                            IconName::Copy
                        })
                        .label(if self.copied {
                            t!("action.copied").into_owned()
                        } else {
                            t!("action.copy").into_owned()
                        })
                        .disabled(self.translation.is_empty())
                        .on_click(cx.listener(|this, _, window, cx| {
                            this.copy_translation(&CopyTranslation, window, cx)
                        })),
                ),
            );

        let status_bar = h_flex()
            .px_3()
            .py_1()
            .border_t_1()
            .border_color(theme.border)
            .text_xs()
            .text_color(theme.muted_foreground)
            .gap_2()
            .child(
                div()
                    .id("status")
                    .flex_1()
                    .min_w_0()
                    .when(!show_setup, |status| status.child(self.status_text()))
                    .when(
                        matches!(self.status, Status::Done { .. } | Status::Cached { .. })
                            && !self.cuts.is_empty(),
                        |this| {
                            this.cursor_pointer()
                                .on_click(cx.listener(|this, _, _, cx| this.show_next_cut(cx)))
                        },
                    ),
            )
            .when(matches!(self.status, Status::LicenseRequired), |bar| {
                bar.child(
                    Button::new("review-model-license")
                        .ghost()
                        .xsmall()
                        .label(t!("setup.agree_use").into_owned())
                        .on_click(
                            cx.listener(|this, _, window, cx| this.accept_license(window, cx)),
                        ),
                )
            })
            .when(matches!(self.status, Status::LicenseRequired), |bar| {
                bar.child(crate::models::plamo_license_button())
            })
            .children(update_button)
            .when(
                !show_setup && self.can_reload_after_runtime_error(),
                |bar| {
                    bar.child(
                        Button::new("reload-model-after-runtime-error")
                            .ghost()
                            .xsmall()
                            .label(t!("models.reload").into_owned())
                            .on_click(
                                cx.listener(|this, _, window, cx| this.start_load(window, cx)),
                            ),
                    )
                },
            )
            .when(
                !show_setup
                    && matches!(self.status, Status::Error(_))
                    && self.all_installed_models_failed(cx),
                |bar| {
                    bar.child(
                        Button::new("manage-failed-models")
                            .ghost()
                            .xsmall()
                            .label(t!("models.manage").into_owned())
                            .on_click(
                                cx.listener(|this, _, window, cx| this.open_models(window, cx)),
                            ),
                    )
                },
            )
            .when(!show_setup, |bar| bar.child(model_menu))
            .child(settings_button);

        v_flex()
            .key_context(KEY_CONTEXT)
            .on_drop(cx.listener(|this, paths: &ExternalPaths, window, cx| {
                if this.current_job.is_none() && !this.setup_busy(cx) {
                    if !this.file_tab {
                        this.setup_attempt = None;
                    }
                    this.accept_files(paths.paths().to_vec(), window, cx);
                }
            }))
            .on_action(cx.listener(Self::copy_translation))
            .on_action(cx.listener(Self::translate_now_action))
            .size_full()
            .bg(theme.background)
            .text_color(theme.foreground)
            .child(title_bar)
            .child(
                h_flex()
                    .px_3()
                    .py_1()
                    .gap_2()
                    .child(
                        Button::new("text-tab")
                            .ghost()
                            .selected(!self.file_tab)
                            .disabled(setup_busy)
                            .label(t!("files.text_tab").into_owned())
                            .on_click(cx.listener(|this, _, _, cx| {
                                if this.file_tab {
                                    this.setup_attempt = None;
                                }
                                this.file_tab = false;
                                cx.notify();
                            })),
                    )
                    .child(
                        Button::new("file-tab")
                            .ghost()
                            .selected(self.file_tab)
                            .disabled(setup_busy)
                            .label(t!("files.file_tab").into_owned())
                            .on_click(cx.listener(|this, _, _, cx| {
                                if !this.file_tab {
                                    this.setup_attempt = None;
                                }
                                this.file_tab = true;
                                cx.notify();
                            })),
                    ),
            )
            .when(!show_setup, |root| root.child(toolbar))
            .when_some(self.model_notice.clone(), |this, notice| {
                this.child(
                    div()
                        .px_3()
                        .py_1()
                        .text_sm()
                        .bg(theme.warning.opacity(0.12))
                        .text_color(theme.foreground)
                        .child(notice),
                )
            })
            .when(self.jobs.busy() && !self.file_tab, |this| {
                this.child(
                    div()
                        .px_3()
                        .py_1()
                        .child(t!("files.text_deferred").into_owned()),
                )
            })
            .child(if missing_models_folder || self.load_recovery.is_some() {
                self.render_model_recovery(missing_models_folder, cx)
                    .into_any_element()
            } else if show_setup {
                self.render_setup(cx).into_any_element()
            } else if self.file_tab {
                v_flex()
                    .flex_1()
                    .min_h_0()
                    .child(self.render_files(cx))
                    .when_some(
                        self.required_model
                            .filter(|_| self.required_format.is_some() && self.batch.visible()),
                        |panel, entry| panel.child(self.render_download(entry, None, cx)),
                    )
                    .into_any_element()
            } else {
                h_flex()
                    .flex_1()
                    .min_h_0()
                    .px_3()
                    .pb_2()
                    .gap_3()
                    .child(source_pane)
                    .child(output_pane)
                    .into_any_element()
            })
            .when(self.show_options && !show_setup, |this| {
                this.child(
                    h_flex()
                        .px_3()
                        .pb_2()
                        .gap_3()
                        .child(
                            v_flex()
                                .flex_1()
                                .min_w_0()
                                .gap_1()
                                .child(
                                    div()
                                        .text_xs()
                                        .text_color(muted)
                                        .child(t!("options.context").into_owned()),
                                )
                                .map(|panel| match fixed {
                                    Some(s) => panel.child(
                                        div()
                                            .h(scaled(96.))
                                            .id("fixed-context")
                                            .overflow_y_scroll()
                                            .child(s.context.clone()),
                                    ),
                                    None => panel
                                        .child(Textarea::new(&self.context_input).h(scaled(96.))),
                                }),
                        )
                        .child(
                            v_flex()
                                .flex_1()
                                .min_w_0()
                                .gap_1()
                                .child(
                                    div()
                                        .text_xs()
                                        .text_color(muted)
                                        .child(t!("options.glossary").into_owned()),
                                )
                                .map(|panel| match fixed {
                                    Some(s) => panel.child(
                                        div()
                                            .h(scaled(96.))
                                            .id("fixed-glossary")
                                            .overflow_y_scroll()
                                            .child(
                                                s.glossary
                                                    .iter()
                                                    .map(|e| format!("{} → {}", e.source, e.target))
                                                    .collect::<Vec<_>>()
                                                    .join("\n"),
                                            ),
                                    ),
                                    None => panel
                                        .child(Textarea::new(&self.glossary_input).h(scaled(96.))),
                                }),
                        ),
                )
            })
            .child(status_bar)
    }
}

/// A short label such as `2B · Q4_K_M` for the standard file names, else the file stem.
fn model_label(path: &Path) -> String {
    let stem = path
        .file_stem()
        .map(|s| s.to_string_lossy().into_owned())
        .unwrap_or_default();
    match stem
        .strip_prefix("plamo-3-translate-")
        .and_then(|rest| rest.split_once('-'))
    {
        Some((size, quant)) => format!("{} · {quant}", size.to_uppercase()),
        None => stem,
    }
}

#[cfg(test)]
mod tests {
    // Import explicitly: `gpui_kit::*` exports a `test` macro that would shadow `#[test]`.
    use std::path::Path;

    use super::{ModelChoice, ReplaceStep, model_label};

    #[test]
    fn startup_probes_the_installation_even_when_automatic_checks_are_off() {
        use super::UpdateCheckTrigger::{Automatic, Manual, Startup};

        assert!(Startup.probes_installation());
        assert!(!Startup.can_check_network(false));
        assert!(Startup.can_check_network(true));
        assert!(!Automatic.probes_installation());
        assert!(!Manual.probes_installation());
        assert!(Manual.can_check_network(false));
    }

    #[test]
    fn disabled_release_updates_never_start_the_check_or_installation_probe() {
        super::if_updates_enabled(false, || {
            panic!("disabled checks must not start update work")
        });
    }

    #[test]
    fn only_the_request_phase_mentions_github() {
        use rust_i18n::t;

        use super::UpdateCheck;

        let local = UpdateCheck::Checking.checking_key().unwrap();
        let request = UpdateCheck::Requesting.checking_key().unwrap();
        assert_eq!(t!(local, locale = "en"), "Checking…");
        assert_eq!(t!(local, locale = "ja"), "確認中…");
        assert_eq!(t!(request, locale = "en"), "Checking GitHub…");
        assert_eq!(t!(request, locale = "ja"), "GitHub に確認中…");
        for locale in ["zh-Hans", "zh-Hant", "ko", "es", "fr", "de", "pt-BR", "id"] {
            assert!(!t!(local, locale = locale).contains("GitHub"));
            assert!(t!(request, locale = locale).contains("GitHub"));
            assert_ne!(t!(local, locale = locale), t!(local, locale = "en"));
            assert_ne!(t!(request, locale = locale), t!(request, locale = "en"));
        }
    }

    #[test]
    fn detected_source_label_requires_a_matching_request_source() {
        use super::{ModelProfile, SourceChoice, lang, shown_detected_source};

        let detect = SourceChoice::Detect;
        let detected = Some("zh-hans");
        let hy = lang::routed_source(ModelProfile::HyMt2, detect, detected);
        let plamo = lang::routed_source(ModelProfile::PlamoPfml, detect, detected);

        assert_eq!(shown_detected_source(detect, detected, hy.code()), detected);
        assert_eq!(shown_detected_source(detect, detected, plamo.code()), None);
        assert_eq!(shown_detected_source(detect, detected, Some("es")), None);
        assert_eq!(shown_detected_source(detect, None, hy.code()), None);
        assert_eq!(shown_detected_source(hy, detected, hy.code()), None);
    }

    #[test]
    fn hy_mt2_catalog_models_retain_their_document_formats() {
        use engine::profiles::FileFormat;

        use super::{ModelProfile, Settings, catalog, file_capabilities};

        let mut settings = Settings {
            model: Some(catalog::HY_MT2_7B.id().into()),
            model_fixed: true,
            ..Settings::default()
        };
        let profile = ModelProfile::HyMt2;
        let smaller = ModelChoice::Catalog(&catalog::HY_MT2);
        for format in [FileFormat::Word, FileFormat::PowerPoint, FileFormat::Pdf] {
            assert!(
                file_capabilities(None, &settings, profile)
                    .files
                    .contains(&format)
            );
            assert!(
                file_capabilities(Some(&smaller), &settings, profile)
                    .files
                    .contains(&format)
            );
        }
        settings.model = Some(catalog::HY_MT2.id().into());
        assert!(
            file_capabilities(None, &settings, profile).files
                == catalog::HY_MT2.capabilities().files
        );
        settings.model = Some(catalog::HY_MT2_7B.id().into());
        settings.model_fixed = false;
        assert!(file_capabilities(None, &settings, profile).files == profile.capabilities().files);
    }

    #[test]
    fn runtime_failures_use_load_recovery_only_while_the_model_is_loading() {
        use super::{Status, runtime_failed_during_load};

        assert!(runtime_failed_during_load(&Status::Loading(0.0), 0, true));
        assert!(runtime_failed_during_load(&Status::Ready, 1, true));
        assert!(runtime_failed_during_load(
            &Status::Measuring {
                first: true,
                step: 0,
                count: 1
            },
            0,
            true,
        ));
        assert!(!runtime_failed_during_load(&Status::Loading(0.0), 1, false));
        assert!(!runtime_failed_during_load(&Status::Ready, 0, true));
        assert!(!runtime_failed_during_load(
            &Status::Translating {
                target: "ja",
                chunk: None
            },
            0,
            true,
        ));
    }

    #[test]
    fn a_second_switch_while_loading_restores_the_last_ready_model() {
        use super::{catalog, rollback_before_switch};

        let working = ModelChoice::Catalog(plamo_2b());
        let first = ModelChoice::Catalog(&catalog::HY_MT2);
        let second = ModelChoice::Catalog(&catalog::HY_MT2_7B);
        let rollback = rollback_before_switch(None, true, Some(&working), &first);
        assert_eq!(
            rollback_before_switch(rollback, false, Some(&first), &second),
            Some(working)
        );
    }

    #[test]
    fn startup_keeps_saved_unconsented_plamo_selected_but_never_loads_it() {
        use super::{ModelStore, Settings, catalog, model_to_load, startup_model};

        let path = std::env::temp_dir().join(format!("plamo-startup-test-{}", std::process::id()));
        std::fs::create_dir(&path).unwrap();
        let store = ModelStore::new(&path);
        let entry = catalog::default_entry();
        let mut settings = Settings {
            model: Some(entry.id().into()),
            license_accepted: false,
            ..Settings::default()
        };
        assert!(startup_model(&settings, &store).is_none());
        let file = std::fs::File::create(store.path(entry)).unwrap();
        file.set_len(entry.bytes).unwrap();
        assert_eq!(startup_model(&settings, &store).unwrap().id(), entry.id());
        assert!(model_to_load(&settings, &store).is_none());
        settings.model = None;
        assert!(startup_model(&settings, &store).is_none());
        settings.license_accepted = true;
        assert_eq!(model_to_load(&settings, &store).unwrap().id(), entry.id());
        std::fs::remove_dir_all(path).unwrap();
    }

    #[test]
    fn labels_standard_model_files() {
        assert_eq!(
            model_label(Path::new("/m/plamo-3-translate-2b-Q4_K_M.gguf")),
            "2B · Q4_K_M"
        );
        assert_eq!(
            model_label(Path::new("plamo-3-translate-0.8b-Q2_K.gguf")),
            "0.8B · Q2_K"
        );
        assert_eq!(model_label(Path::new("custom.gguf")), "custom");
    }

    /// A revision can have the same ID as the loaded model. Release the file before replacing it.
    #[test]
    fn a_new_revision_of_the_model_in_use_is_reloaded() {
        use engine::models::{BUILTIN, CatalogEntry};
        let old = BUILTIN
            .iter()
            .find(|e| e.size == "2b" && e.quant == "Q4_K_M")
            .unwrap();
        let newer: &'static CatalogEntry = Box::leak(Box::new(CatalogEntry {
            revision: "0000000000000000000000000000000000000000",
            bytes: old.bytes + 1,
            ..*old
        }));
        assert_eq!(old.id(), newer.id());
        assert_ne!(old, newer);
        assert_eq!(
            ReplaceStep::of(false, Some(&ModelChoice::Catalog(old)), newer),
            ReplaceStep::Reload
        );
    }

    fn plamo_2b() -> &'static engine::models::CatalogEntry {
        engine::models::BUILTIN
            .iter()
            .find(|e| e.size == "2b" && e.quant == "Q4_K_M")
            .unwrap()
    }

    /// Defer replacing the PLaMo file while the worker is reloading it after a failed switch.
    /// The models list can have no active row during that handoff.
    #[test]
    fn an_update_verified_while_its_model_loads_waits_for_the_release() {
        use super::Releases;
        let plamo = plamo_2b();
        let loading = ModelChoice::Catalog(plamo);
        // A load, a measurement or a text translation is no file job: the model is released.
        assert_eq!(
            ReplaceStep::of(false, Some(&loading), plamo),
            ReplaceStep::Reload
        );
        let mut releases = Releases::default();
        releases.requested();
        releases.wait_for_last(plamo);
        // The worker answers after the load: then the file is installed and loaded again.
        let (installs, reload) = releases.released(None);
        assert_eq!((installs, reload), (vec![plamo], Some(plamo)));
        // A file job keeps the model: the update waits for it, and nothing is installed.
        assert_eq!(
            ReplaceStep::of(true, Some(&loading), plamo),
            ReplaceStep::Wait
        );
    }

    /// Completing an unload for an update must preserve a newer model choice.
    #[test]
    fn a_choice_made_while_an_update_waits_is_kept() {
        use super::Releases;
        use engine::models::HY_MT2;
        let plamo = plamo_2b();
        for (chosen, reload) in [
            // Nothing chosen meanwhile: the updated model is loaded again, as on main.
            (None, Some(plamo)),
            // Another model loads: it stays, and only the file is installed.
            (Some(ModelChoice::Catalog(&HY_MT2)), None),
            // The same model chosen again may have opened the older file: loaded again.
            (Some(ModelChoice::Catalog(plamo)), Some(plamo)),
        ] {
            let mut releases = Releases::default();
            releases.requested();
            releases.wait_for_last(plamo);
            assert_eq!(
                releases.released(chosen.as_ref()),
                (vec![plamo], reload),
                "{chosen:?}"
            );
        }
    }

    /// An update waits for the unload request sent when it arrived, not for an
    /// earlier one that the worker answers before it has released the later model.
    #[test]
    fn an_update_waits_for_its_own_unload_request() {
        use super::Releases;
        use engine::models::{HY_MT2, HY_MT2_7B};
        for entry in [&HY_MT2, &HY_MT2_7B] {
            let mut releases = Releases::default();
            // The user unloads PLaMo, chooses Hy-MT2, and its update arrives: a second unload.
            releases.requested();
            releases.requested();
            releases.wait_for_last(entry);
            let hy_mt2 = ModelChoice::Catalog(entry);
            assert_eq!(releases.released(Some(&hy_mt2)), (vec![], None));
            assert_eq!(releases.released(None), (vec![entry], Some(entry)));
            assert_eq!(releases.released(None), (vec![], None));
        }
    }

    /// A later download must preserve an earlier update waiting to be installed.
    #[test]
    fn every_waiting_update_is_kept() {
        use super::Releases;
        use engine::models::{CatalogEntry, HY_MT2, INDEX_TRANSLATE};
        let plamo = plamo_2b();
        let newer: &'static CatalogEntry = Box::leak(Box::new(CatalogEntry {
            revision: "0000000000000000000000000000000000000000",
            ..*plamo
        }));
        let mut releases = Releases::default();
        for entry in [plamo, &HY_MT2, newer] {
            releases.defer(entry);
        }
        // The newer revision of the same file replaces the older one.
        assert_eq!(releases.take_deferred(), vec![&HY_MT2, newer]);
        assert!(releases.take_deferred().is_empty());
        releases.requested();
        releases.wait_for_last(&HY_MT2);
        releases.wait_for_last(&INDEX_TRANSLATE);
        assert_eq!(releases.released(None).0, vec![&HY_MT2, &INDEX_TRANSLATE]);
    }

    /// During a reload, preserve a selected device change unless a file job holds the model.
    #[test]
    fn only_a_file_job_keeps_the_model_and_its_device() {
        use crate::jobs::Jobs;
        // A load, a measurement or a text translation leaves the jobs idle.
        let mut jobs = Jobs::default();
        assert!(!super::keeps_model(&jobs));
        jobs.start_file(1);
        assert!(super::keeps_model(&jobs));
        jobs.finish_file(1);
        jobs.start_batch();
        assert!(super::keeps_model(&jobs));
    }

    /// A single BabelDOC run keeps the model through its paragraphs'
    /// completions (a verified update waits), and releases it only at the run's end, which
    /// then installs what waited (`external_done` calls `install_deferred_model_updates`).
    #[test]
    fn a_babeldoc_run_keeps_the_model_until_its_end() {
        use crate::jobs::Jobs;
        let mut jobs = Jobs::default();
        jobs.start_external();
        assert!(super::keeps_model(&jobs));
        // The last paragraph's `Finished` arrives while the run still holds the worker.
        assert_eq!(jobs.finish_file(9), None);
        assert!(super::keeps_model(&jobs));
        jobs.finish_external();
        assert!(!super::keeps_model(&jobs));
    }
}

/// Ctrl+Enter (Cmd+Enter) in a text input, with the bindings as the app sets them up.
#[cfg(test)]
mod shortcut_tests {
    use gpui::{TestAppContext, VisualTestContext};
    use gpui_kit::component::input::{Textarea, TextareaState};
    use gpui_kit::prelude::*;
    use gpui_kit::{Context, Entity, Window, WindowOptions, div};

    use super::{KEY_CONTEXT, TRANSLATE_NOW_KEY, TranslateNow, bind_keys};

    /// A text input under the window's key context, counting the requests to translate.
    struct Probe {
        text: Entity<TextareaState>,
        translations: usize,
    }

    impl Render for Probe {
        fn render(&mut self, _: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
            div()
                .key_context(KEY_CONTEXT)
                .on_action(cx.listener(|this, _: &TranslateNow, _, _| this.translations += 1))
                .size_full()
                .child(Textarea::new(&self.text).w_full())
        }
    }

    #[gpui::test]
    fn translating_with_the_shortcut_adds_no_line_break(cx: &mut TestAppContext) {
        cx.update(|cx| {
            gpui_kit::init(cx);
            bind_keys(cx);
        });
        let (window, probe) = cx.update(|cx| {
            gpui_kit::open_window(WindowOptions::default(), cx, |window, cx| {
                cx.new(|cx| Probe {
                    text: cx.new(|cx| TextareaState::new(window, cx)),
                    translations: 0,
                })
            })
            .expect("open the test window")
        });
        let text = cx.update(|cx| probe.read(cx).text.clone());
        let mut cx = VisualTestContext::from_window(window, cx);
        cx.update(|window, cx| text.update(cx, |state, cx| state.focus(window, cx)));
        cx.run_until_parked();
        cx.simulate_input("Hello");
        cx.simulate_keystrokes(TRANSLATE_NOW_KEY);
        cx.update(|_, cx| assert_eq!(text.read(cx).value().to_string(), "Hello"));
        cx.update(|_, cx| assert_eq!(probe.read(cx).translations, 1));
        // Enter alone still breaks the line, and translates nothing.
        cx.simulate_keystrokes("enter");
        cx.update(|_, cx| assert_eq!(text.read(cx).value().to_string(), "Hello\n"));
        cx.update(|_, cx| assert_eq!(probe.read(cx).translations, 1));
    }
}
