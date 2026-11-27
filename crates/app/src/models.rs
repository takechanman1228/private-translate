//! The model manager: catalog, downloads and the license gate.

use std::collections::HashMap;
use std::path::PathBuf;

use engine::discovery;
use engine::download::{DownloadEvent, Downloads, Progress};
use engine::models::{self as catalog, CatalogEntry, LocalState, ModelStore};
use engine::profiles::{FileFormat, ModelLicense, ModelProfile};
use engine::selection;
use gpui_kit::component::button::{Button, ButtonVariant, ButtonVariants as _};
use gpui_kit::component::progress::Progress as ProgressBar;
use gpui_kit::component::scroll::ScrollableElement as _;
use gpui_kit::component::tag::Tag;
use gpui_kit::component::{
    ActiveTheme as _, Disableable as _, Sizable as _, WindowExt as _, h_flex, v_flex,
};
use gpui_kit::prelude::FluentBuilder as _;
use gpui_kit::*;
use rust_i18n::t;

use crate::view::{scaled, scaled_px};
use crate::{lang, setup};

/// Where the quality figures come from.
const QUALITY_SOURCE_URL: &str = "https://huggingface.co/pfnet/plamo-3-translate-2b-GGUF";

/// The license is published in English and Japanese.
fn license_url() -> String {
    t!("models.license_url").into_owned()
}

fn file_stamp(path: &std::path::Path) -> Option<(u64, std::time::SystemTime)> {
    let metadata = path.metadata().ok()?;
    Some((metadata.len(), metadata.modified().ok()?))
}

pub enum ModelsEvent {
    /// The user picked a model to pin.
    Use(&'static CatalogEntry),
    /// A setup or inline action requested a load without pinning the model.
    UseOnce(&'static CatalogEntry),
    /// A download finished without an explicit request to load it.
    Downloaded(&'static CatalogEntry),
    /// Release the active model.
    Unload,
    /// Delete the active model. The view unloads it first, then calls [`ModelsPanel::delete`].
    DeleteActive(&'static CatalogEntry),
    /// A download was verified beside a file of the same name (an older revision, or a damaged
    /// copy). The view installs it based on the file the worker holds: at once with
    /// [`ModelsPanel::install_verified`] if the file is not in use, else after unloading the
    /// model, calling [`ModelsPanel::replace`] and loading it again.
    Verified(&'static CatalogEntry),
    /// Keep models in this folder (the default one for `None`). The view saves it and calls
    /// [`ModelsPanel::set_store`].
    ChangeFolder(Option<PathBuf>),
    /// Record consent given through a download or use action.
    AcceptLicense,
    /// Entries were added or updated from Hugging Face.
    CatalogUpdated,
}

/// Result of checking Hugging Face for newly published files.
enum CatalogStatus {
    Checking,
    UpToDate,
    Updated(usize),
    Failed(String),
}

pub struct ModelsPanel {
    downloads: Downloads,
    details: bool,
    progress: HashMap<&'static str, Progress>,
    errors: HashMap<&'static str, (String, setup::Failure)>,
    load_errors: HashMap<&'static str, (String, bool)>,
    failed_files: HashMap<&'static str, (u64, std::time::SystemTime)>,
    active: Option<&'static str>,
    recommended: Option<&'static CatalogEntry>,
    system_memory: u64,
    license_accepted: bool,
    /// Load this first-setup model once its download is verified.
    use_when_done: Option<&'static str>,
    catalog_status: CatalogStatus,
    catalog_cache: PathBuf,
    /// Why the folder picker failed (on Linux without a desktop portal).
    folder_error: Option<String>,
    _events: Task<()>,
    _refresh: Task<()>,
}

impl EventEmitter<ModelsEvent> for ModelsPanel {}

impl ModelsPanel {
    pub fn new(
        store: ModelStore,
        recommended: Option<&'static CatalogEntry>,
        license_accepted: bool,
        catalog_cache: PathBuf,
        cx: &mut Context<Self>,
    ) -> Self {
        let (tx, rx) = async_channel::unbounded();
        let downloads = Downloads::new(store, move |event| {
            let _ = tx.send_blocking(event);
        });
        let events = cx.spawn(async move |this, cx| {
            while let Ok(event) = rx.recv().await {
                if this
                    .update(cx, |this, cx| this.on_download_event(event, cx))
                    .is_err()
                {
                    break;
                }
            }
        });
        let mut this = Self {
            downloads,
            details: false,
            progress: HashMap::new(),
            errors: HashMap::new(),
            load_errors: HashMap::new(),
            failed_files: HashMap::new(),
            active: None,
            recommended,
            system_memory: 0,
            license_accepted,
            use_when_done: None,
            catalog_status: CatalogStatus::Checking,
            catalog_cache,
            folder_error: None,
            _events: events,
            _refresh: Task::ready(()),
        };
        this.refresh_catalog(cx);
        // Partial downloads of revisions no longer in the catalog can never be resumed (PFN
        // stops serving replaced files), and can be gigabytes.
        let store = this.store().clone();
        cx.background_spawn(async move {
            for entry in catalog::entries() {
                let freed = store.remove_stale_partials(entry);
                if freed > 0 {
                    crate::app_log!(
                        "removed a partial download of an older revision of {} ({freed} bytes)",
                        entry.id()
                    );
                }
            }
        })
        .detach();
        this
    }

    /// Looks for newly published files on Hugging Face in the background.
    fn refresh_catalog(&mut self, cx: &mut Context<Self>) {
        self.catalog_status = CatalogStatus::Checking;
        let cache = self.catalog_cache.clone();
        self._refresh = cx.spawn(async move |this, cx| {
            let result = cx
                .background_spawn(async move {
                    let found = discovery::discover()?;
                    if let Err(e) = discovery::save_cache(&cache, &found) {
                        crate::app_log!("cannot save the catalog cache: {e}");
                    }
                    Ok::<_, discovery::DiscoveryError>(found)
                })
                .await;
            let _ = this.update(cx, |this, cx| {
                this.catalog_status = match result {
                    Ok(found) => {
                        let total = found.len();
                        let changed = catalog::register(found.into_iter().map(|d| d.into_entry()));
                        crate::app_log!(
                            "catalog: {total} files on Hugging Face, {changed} new or updated"
                        );
                        if changed > 0 {
                            cx.emit(ModelsEvent::CatalogUpdated);
                            CatalogStatus::Updated(changed)
                        } else {
                            CatalogStatus::UpToDate
                        }
                    }
                    Err(e) => {
                        crate::app_log!("catalog check failed: {e}");
                        CatalogStatus::Failed(e.to_string())
                    }
                };
                cx.notify();
            });
        });
    }

    pub fn details(&self) -> bool {
        self.details
    }

    fn agree(&mut self, entry: &CatalogEntry, cx: &mut Context<Self>) {
        if entry.is_plamo() && !self.license_accepted {
            self.license_accepted = true;
            cx.emit(ModelsEvent::AcceptLicense);
        }
    }

    pub fn store(&self) -> &ModelStore {
        self.downloads.store()
    }

    /// Keeps models in another folder. No download may be running.
    pub fn set_store(&mut self, store: ModelStore, cx: &mut Context<Self>) {
        self.downloads.set_store(store);
        self.errors.clear();
        self.load_errors.clear();
        self.failed_files.clear();
        self.use_when_done = None;
        self.folder_error = None;
        cx.notify();
    }

    /// Asks for the folder to keep models in.
    pub fn choose_folder(&mut self, cx: &mut Context<Self>) {
        if !self.progress.is_empty() {
            return;
        }
        self.folder_error = None;
        let paths = cx.prompt_for_paths(PathPromptOptions {
            files: false,
            directories: true,
            multiple: false,
            prompt: Some(t!("models.choose_folder").into_owned().into()),
        });
        cx.spawn(async move |this, cx| {
            let result = paths.await;
            let _ = this.update(cx, |this, cx| match result {
                Ok(Ok(Some(paths))) => {
                    if let Some(dir) = paths.into_iter().next() {
                        cx.emit(ModelsEvent::ChangeFolder(Some(dir)));
                    }
                }
                Ok(Err(e)) => {
                    crate::app_log!("the folder picker failed: {e}");
                    this.folder_error = Some(e.to_string());
                    cx.notify();
                }
                // Cancelled.
                Ok(Ok(None)) | Err(_) => {}
            });
        })
        .detach();
    }

    pub fn folder_picker_error(&self) -> Option<&str> {
        self.folder_error.as_deref()
    }

    pub fn set_license_accepted(&mut self, accepted: bool, cx: &mut Context<Self>) {
        self.license_accepted = accepted;
        cx.notify();
    }

    pub fn set_recommended(
        &mut self,
        entry: Option<&'static CatalogEntry>,
        system_memory: u64,
        cx: &mut Context<Self>,
    ) {
        self.system_memory = system_memory;
        self.recommended = entry;
        cx.notify();
    }

    pub fn set_active(&mut self, entry: Option<&'static CatalogEntry>, cx: &mut Context<Self>) {
        self.active = entry.map(CatalogEntry::id);
        if let Some(id) = self.active {
            self.load_errors.remove(id);
            self.failed_files.remove(id);
        }
        // A model chosen meanwhile is not replaced by a download finishing later.
        if self.active.is_some() {
            self.use_when_done = None;
        }
        cx.notify();
    }

    pub fn record_load_failure(
        &mut self,
        entry: &CatalogEntry,
        error: String,
        invalid_file: bool,
        cx: &mut Context<Self>,
    ) {
        let error = if invalid_file {
            t!("models.invalid_file").into_owned()
        } else {
            error
        };
        self.load_errors.insert(entry.id(), (error, invalid_file));
        if let Some(stamp) = file_stamp(&self.store().path(entry)) {
            self.failed_files.insert(entry.id(), stamp);
        }
        cx.notify();
    }

    pub fn available_automatically(&self, entry: &CatalogEntry) -> bool {
        self.store().state(entry).is_usable()
            && self
                .failed_files
                .get(entry.id())
                .is_none_or(|stamp| file_stamp(&self.store().path(entry)).as_ref() != Some(stamp))
    }

    pub fn has_usable_model(&self, except: Option<&CatalogEntry>) -> bool {
        catalog::entries().into_iter().any(|entry| {
            except.is_none_or(|excluded| excluded.id() != entry.id())
                && self.can_download(entry)
                && self.available_automatically(entry)
        })
    }

    pub fn use_downloaded_for_setup(&self, entry: &CatalogEntry, pending: bool) -> bool {
        self.can_download(entry)
            && self.available_automatically(entry)
            && (pending || !self.has_usable_model(Some(entry)))
    }

    pub fn select_automatically(
        &self,
        request: selection::Request<'_>,
        entries: &[&'static CatalogEntry],
    ) -> selection::Selection {
        let downloaded: Vec<_> = entries
            .iter()
            .filter(|entry| self.available_automatically(entry))
            .map(|entry| entry.id())
            .collect();
        selection::select(request, entries, &downloaded)
    }

    pub fn clear_auto_use(&mut self) {
        self.use_when_done = None;
    }

    fn use_after_download(&mut self, entry: &'static CatalogEntry, cx: &mut Context<Self>) {
        if self.use_when_done == Some(entry.id()) {
            self.use_when_done = None;
            cx.emit(ModelsEvent::UseOnce(entry));
        } else {
            cx.emit(ModelsEvent::Downloaded(entry));
        }
    }

    fn on_download_event(&mut self, event: DownloadEvent, cx: &mut Context<Self>) {
        match event {
            DownloadEvent::Progress { entry, progress } => {
                self.progress.insert(entry.id(), progress);
            }
            DownloadEvent::Finished { entry, path } => {
                crate::app_log!("download finished: {}", path.display());
                self.progress.remove(entry.id());
                self.load_errors.remove(entry.id());
                self.failed_files.remove(entry.id());
                self.use_after_download(entry, cx);
            }
            DownloadEvent::Verified { entry } => {
                crate::app_log!("downloaded beside an older revision: {}", entry.id());
                self.progress.remove(entry.id());
                // This panel tracks the last loaded model. The view also knows which file the
                // worker is loading or still holds.
                cx.emit(ModelsEvent::Verified(entry));
            }
            DownloadEvent::Cancelled { entry } => {
                crate::app_log!("download cancelled: {}", entry.id());
                self.progress.remove(entry.id());
                if self.use_when_done == Some(entry.id()) {
                    self.use_when_done = None;
                }
            }
            DownloadEvent::Failed { entry, error } => {
                crate::app_log!("download failed: {}: {error}", entry.id());
                self.progress.remove(entry.id());
                let failure = setup::Failure::from_download(&error);
                self.errors.insert(entry.id(), (error.to_string(), failure));
                if self.use_when_done == Some(entry.id()) {
                    self.use_when_done = None;
                }
            }
        }
        cx.notify();
    }

    fn folder_missing(&self) -> bool {
        let dir = self.store().dir();
        setup::configured_folder_missing(
            (dir != crate::settings::default_models_dir()).then_some(dir),
        )
    }

    fn can_download(&self, entry: &CatalogEntry) -> bool {
        self.license_accepted
            || entry.profile.capabilities().license != ModelLicense::PlamoCommunity
    }

    pub fn setup_progress(&self, entry: &CatalogEntry) -> Option<Progress> {
        self.progress.get(entry.id()).copied()
    }

    pub fn setup_download(&self) -> Option<&'static CatalogEntry> {
        self.progress.keys().find_map(|id| catalog::find_entry(id))
    }

    pub fn setup_error(&self, entry: &CatalogEntry) -> Option<&str> {
        self.errors
            .get(entry.id())
            .map(|(error, _)| error)
            .or_else(|| self.load_errors.get(entry.id()).map(|(error, _)| error))
            .map(String::as_str)
    }

    pub fn setup_failure(&self, entry: &CatalogEntry) -> Option<setup::Failure> {
        self.errors
            .get(entry.id())
            .map(|(_, failure)| *failure)
            .or_else(|| {
                self.load_errors
                    .contains_key(entry.id())
                    .then_some(setup::Failure::Load)
            })
    }

    pub fn setup_ready(&self, entry: &CatalogEntry) -> bool {
        self.store().state(entry).is_usable()
            && !self
                .load_errors
                .get(entry.id())
                .is_some_and(|(_, invalid)| *invalid)
    }

    pub fn start_setup(&mut self, entry: &'static CatalogEntry, cx: &mut Context<Self>) {
        if !self.can_download(entry) {
            return;
        }
        if self.setup_ready(entry) {
            cx.emit(ModelsEvent::UseOnce(entry));
        } else if !self.downloads.is_active(entry) {
            self.use_when_done = Some(entry.id());
            self.start(entry, cx);
        }
    }

    pub(super) fn start(&mut self, entry: &'static CatalogEntry, cx: &mut Context<Self>) {
        if !self.can_download(entry) || self.downloads.is_active(entry) || self.folder_missing() {
            return;
        }
        self.errors.remove(entry.id());
        crate::app_log!(
            "download started: {} (local state {:?})",
            entry.id(),
            self.store().state(entry)
        );
        let downloaded = self.store().state(entry).partial().unwrap_or(0);
        self.progress.insert(
            entry.id(),
            Progress {
                downloaded,
                total: entry.bytes,
                verifying: false,
                bytes_per_second: 0.0,
                average_bytes_per_second: 0.0,
            },
        );
        if entry.is_plamo() && self.active.is_none() {
            self.use_when_done = Some(entry.id());
        }
        self.downloads.start(entry);
        cx.notify();
    }

    /// Installs a verified download whose file is not in use (see [`ModelsEvent::Verified`]).
    pub fn install_verified(&mut self, entry: &'static CatalogEntry, cx: &mut Context<Self>) {
        if self.replace(entry, cx) {
            self.use_after_download(entry, cx);
        }
    }

    pub fn cancel(&mut self, entry: &'static CatalogEntry, cx: &mut Context<Self>) {
        self.downloads.cancel(entry);
        cx.notify();
    }

    fn confirm_delete(
        &mut self,
        entry: &'static CatalogEntry,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let panel = cx.entity().downgrade();
        window.open_alert_dialog(cx, move |alert, _, _| {
            let panel = panel.clone();
            alert
                .title(t!("models.delete_title").into_owned())
                .description(
                    t!(
                        "models.delete_body",
                        model = entry.to_string(),
                        size = format_bytes(entry.bytes)
                    )
                    .into_owned(),
                )
                .show_cancel(true)
                .ok_text(t!("models.delete").into_owned())
                .ok_variant(ButtonVariant::Danger)
                .cancel_text(t!("models.cancel").into_owned())
                .on_ok(move |_, _, cx| {
                    let _ = panel.update(cx, |this, cx| {
                        if this.active == Some(entry.id()) {
                            cx.emit(ModelsEvent::DeleteActive(entry));
                        } else {
                            this.delete(entry, "confirmed in the Models dialog", cx);
                        }
                    });
                    true
                })
        });
    }

    /// Deletes the files of a model that is not loaded. Returns whether they are gone; a
    /// failure shows in the model's row.
    pub fn delete(
        &mut self,
        entry: &'static CatalogEntry,
        reason: &str,
        cx: &mut Context<Self>,
    ) -> bool {
        crate::app_log!(
            "deleting model {} ({reason}): {}",
            entry.id(),
            self.store().path(entry).display()
        );
        let deleted = match self.store().delete(entry) {
            Ok(()) => {
                self.errors.remove(entry.id());
                self.load_errors.remove(entry.id());
                self.failed_files.remove(entry.id());
                true
            }
            Err(e) => {
                crate::app_log!("deleting {} failed: {e}", entry.id());
                self.errors
                    .insert(entry.id(), (e.to_string(), setup::Failure::Storage));
                false
            }
        };
        cx.notify();
        deleted
    }

    /// Puts the newest revision of `entry`, downloaded beside the older file, in its place. The
    /// model must not be loaded. Returns whether it is in place; a failure shows in the row (the
    /// another process may still hold the file on Windows), and updating again retries.
    pub fn replace(&mut self, entry: &'static CatalogEntry, cx: &mut Context<Self>) -> bool {
        let replaced = match self.store().replace_with_download(entry) {
            Ok(path) => {
                crate::app_log!("installed verified replacement: {}", path.display());
                self.errors.remove(entry.id());
                self.load_errors.remove(entry.id());
                self.failed_files.remove(entry.id());
                true
            }
            Err(e) => {
                crate::app_log!("replacing {} failed: {e}", entry.id());
                self.errors
                    .insert(entry.id(), (e.to_string(), setup::Failure::Storage));
                false
            }
        };
        cx.notify();
        replaced
    }

    fn render_row(
        &self,
        entry: &'static CatalogEntry,
        compact_details: bool,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let (danger, warning, secondary, muted) = {
            let theme = cx.theme();
            (
                theme.danger,
                theme.warning,
                theme.secondary,
                theme.muted_foreground,
            )
        };
        let id = entry.id();
        let state = self.store().state(entry);
        let hy_mt2_7b = entry.family == "hy-mt2" && entry.size == "7b";
        let low_memory = hy_mt2_7b && hy_mt2_7b_memory_warning(self.system_memory, entry.bytes);
        let progress = self.progress.get(id).copied();
        let load_error = self.load_errors.get(id);
        let error = self
            .errors
            .get(id)
            .map(|(error, _)| error)
            .or_else(|| load_error.map(|(error, _)| error));
        let is_active = self.active == Some(id);
        let consent = entry.is_plamo() && !self.license_accepted;
        let needs_repair = matches!(state, LocalState::Invalid { .. })
            || (state.is_usable() && load_error.is_some_and(|(_, invalid)| *invalid));

        let status: AnyElement = if let Some(p) = progress {
            v_flex()
                .gap_1()
                .child(
                    ProgressBar::new(SharedString::from(format!("progress-{id}")))
                        .color(gpui_kit::rgb(crate::view::ACTION_ACCENT))
                        .value(p.fraction() * 100.0)
                        .w_full(),
                )
                .child(div().text_xs().child(progress_text(&p)))
                .into_any_element()
        } else if let Some(error) = error {
            div()
                .text_color(danger)
                .child(t!("models.failed", message = error).into_owned())
                .into_any_element()
        } else {
            let present = || {
                if is_active {
                    Tag::secondary()
                        .small()
                        .child(t!("models.in_use").into_owned())
                        .into_any_element()
                } else {
                    div()
                        .child(t!("models.downloaded").into_owned())
                        .into_any_element()
                }
            };
            let paused = |n: u64| {
                t!(
                    "models.paused",
                    percent = format!("{:.0}", n as f64 / entry.bytes as f64 * 100.0)
                )
                .into_owned()
            };
            match state {
                LocalState::Downloaded => present(),
                LocalState::Outdated { partial } => {
                    let mut newer = match entry.published_date() {
                        Some(date) => t!("models.newer_dated", date = date),
                        None => t!("models.newer"),
                    }
                    .into_owned();
                    if let Some(n) = partial {
                        newer = format!("{newer} · {}", paused(n));
                    }
                    v_flex()
                        .gap_0p5()
                        .items_start()
                        .child(present())
                        .child(div().text_xs().text_color(muted).child(newer))
                        .into_any_element()
                }
                LocalState::Invalid { .. } => div()
                    .text_color(danger)
                    .child(t!("models.invalid_size").into_owned())
                    .into_any_element(),
                LocalState::Partial(n) => div().child(paused(n)).into_any_element(),
                LocalState::Missing => div().into_any_element(),
            }
        };

        let button = |name: &str| Button::new(SharedString::from(format!("{name}-{id}"))).xsmall();
        // A fixed-width column keeps Delete in the same place whichever action precedes it.
        let actions = h_flex()
            .when(self.details && !compact_details, |row| row.w(scaled(210.)))
            .flex_shrink_0()
            .gap_1()
            .justify_end()
            .map(|this| {
                if progress.is_some() {
                    this.child(
                        button("cancel")
                            .label(t!("models.cancel").into_owned())
                            .on_click(cx.listener(move |this, _, _, cx| this.cancel(entry, cx))),
                    )
                } else if needs_repair {
                    this.child(
                        button("repair")
                            .label(
                                t!(if consent {
                                    "models.agree_download"
                                } else {
                                    "models.repair"
                                })
                                .into_owned(),
                            )
                            .on_click(cx.listener(move |this, _, _, cx| {
                                this.agree(entry, cx);
                                this.start(entry, cx);
                            })),
                    )
                    .child(
                        button("delete")
                            .ghost()
                            .label(t!("models.delete").into_owned())
                            .on_click(cx.listener(move |this, _, window, cx| {
                                this.confirm_delete(entry, window, cx)
                            })),
                    )
                } else {
                    match state {
                        LocalState::Downloaded | LocalState::Outdated { .. } => this
                            .when(matches!(state, LocalState::Outdated { .. }), |this| {
                                this.child(
                                    button("update")
                                        .label(
                                            if consent {
                                                t!("models.agree_download")
                                            } else if error.is_some() {
                                                t!("models.retry")
                                            } else {
                                                t!("models.update")
                                            }
                                            .into_owned(),
                                        )
                                        // What the newer version changed.
                                        .when_some(entry.change, |this, change| {
                                            this.tooltip(change)
                                        })
                                        .on_click(cx.listener(move |this, _, _, cx| {
                                            this.agree(entry, cx);
                                            this.start(entry, cx);
                                        })),
                                )
                            })
                            .when(is_active && self.details, |this| {
                                this.child(
                                    button("unload")
                                        .label(t!("models.unload").into_owned())
                                        .on_click(
                                            cx.listener(|_, _, _, cx| cx.emit(ModelsEvent::Unload)),
                                        ),
                                )
                            })
                            .when(!is_active, |row| {
                                row.child(
                                    button("use")
                                        .label(
                                            t!(if consent {
                                                "setup.agree_use"
                                            } else {
                                                "models.use"
                                            })
                                            .into_owned(),
                                        )
                                        .on_click(cx.listener(move |this, _, _, cx| {
                                            this.agree(entry, cx);
                                            cx.emit(ModelsEvent::Use(entry))
                                        })),
                                )
                            })
                            .child(
                                button("delete")
                                    .ghost()
                                    .label(t!("models.delete").into_owned())
                                    .on_click(cx.listener(move |this, _, window, cx| {
                                        this.confirm_delete(entry, window, cx)
                                    })),
                            ),
                        LocalState::Partial(_) => this
                            .child(
                                button("resume")
                                    .label(
                                        t!(if consent {
                                            "models.agree_download"
                                        } else {
                                            "models.resume"
                                        })
                                        .into_owned(),
                                    )
                                    .on_click(cx.listener(move |this, _, _, cx| {
                                        this.agree(entry, cx);
                                        this.start(entry, cx);
                                    })),
                            )
                            .child(
                                button("delete")
                                    .ghost()
                                    .label(t!("models.delete").into_owned())
                                    .on_click(cx.listener(move |this, _, window, cx| {
                                        this.confirm_delete(entry, window, cx)
                                    })),
                            ),
                        LocalState::Invalid { .. } | LocalState::Missing => this.child(
                            button("download")
                                .label(
                                    if consent {
                                        t!("models.agree_download")
                                    } else if error.is_some() {
                                        t!("models.retry")
                                    } else {
                                        t!("models.download")
                                    }
                                    .into_owned(),
                                )
                                .on_click(cx.listener(move |this, _, _, cx| {
                                    this.agree(entry, cx);
                                    this.start(entry, cx);
                                })),
                        ),
                    }
                }
            });

        let name = v_flex()
            .w(scaled(if self.details { 230. } else { 190. }))
            .flex_shrink_0()
            .child(
                h_flex()
                    .flex_wrap()
                    .gap_2()
                    .child(
                        div()
                            .font_weight(FontWeight::MEDIUM)
                            .child(if self.details {
                                entry.quant.to_owned()
                            } else {
                                entry.name.map_or_else(
                                    || format!("PLaMo {}", entry.size_label()),
                                    str::to_owned,
                                )
                            }),
                    )
                    .when(
                        !self.details && (hy_mt2_7b || entry.is_plamo() && entry.size == "8b"),
                        |row| {
                            row.child(
                                Tag::secondary()
                                    .small()
                                    .child(t!("setup.high").into_owned()),
                            )
                        },
                    )
                    .when(
                        entry.is_plamo() && entry.size == "2b" && !self.details
                            || self
                                .recommended
                                .is_some_and(|recommended| recommended.id() == entry.id()),
                        |this| {
                            this.child(
                                Tag::secondary().small().child(
                                    t!(if entry.is_plamo() && entry.size == "2b" {
                                        "models.japanese_recommended"
                                    } else {
                                        "models.recommended"
                                    })
                                    .into_owned(),
                                ),
                            )
                        },
                    ),
            )
            .when_some(
                (self.details && entry.is_plamo())
                    .then(|| quant_note(entry.quant))
                    .flatten(),
                |this, note| this.child(div().text_xs().text_color(muted).child(note)),
            );
        let size = div()
            .w(scaled(70.))
            .flex_shrink_0()
            .text_color(muted)
            .child(format_bytes(entry.bytes));
        let info = if !self.details {
            v_flex()
                .text_xs()
                .flex_1()
                .min_w_0()
                .child(model_support(entry, &rust_i18n::locale()))
                .when(entry.is_plamo(), |cell| {
                    cell.child(div().text_color(muted).child("Built with PLaMo"))
                })
                .when(low_memory, |cell| {
                    cell.child(
                        div()
                            .text_color(warning)
                            .child(t!("models.hy_mt2_7b_low_memory").into_owned()),
                    )
                })
                .into_any_element()
        } else if hy_mt2_7b {
            v_flex()
                .w(scaled(170.))
                .flex_shrink_0()
                .text_xs()
                .text_color(muted)
                .child(t!("models.hy_mt2_7b_quality").into_owned())
                .when(low_memory, |cell| {
                    cell.child(
                        div()
                            .text_color(warning)
                            .child(t!("models.hy_mt2_7b_low_memory").into_owned()),
                    )
                })
                .into_any_element()
        } else {
            quality_cell(entry, muted, warning, danger)
        };
        let status = div()
            .when(self.details, |status| status.flex_1())
            .min_w_0()
            .text_sm()
            .child(status);

        h_flex()
            .id(SharedString::from(format!("row-{id}")))
            .gap_2()
            .px_2()
            .py_1p5()
            .rounded_md()
            .when(is_active, |this| this.bg(secondary))
            .map(|row| {
                if compact_details {
                    row.flex_col()
                        .items_stretch()
                        .child(h_flex().gap_2().child(name).child(size).child(info))
                        .child(h_flex().gap_2().child(status).child(actions))
                } else {
                    row.child(name)
                        .child(size)
                        .child(info)
                        .child(status)
                        .child(actions)
                }
            })
            .into_any_element()
    }
}

impl Render for ModelsPanel {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let folder_missing = self.folder_missing();
        let compact_details = self.details && window.viewport_size().width < scaled_px(900., cx);
        let (muted, divider) = {
            let theme = cx.theme();
            (theme.muted_foreground, theme.border)
        };
        // Rows grouped by model size, each group under a header.
        let mut rows: Vec<AnyElement> = Vec::new();
        let mut current_base = String::new();
        let mut entries = catalog::entries();
        let installed_groups: std::collections::HashSet<_> = entries
            .iter()
            .filter(|entry| self.store().state(entry).is_usable())
            .map(|entry| entry.base_model())
            .collect();
        entries.sort_by_cached_key(|entry| {
            (
                entry.is_plamo(),
                if entry.is_plamo() {
                    entry.params_billion().to_bits()
                } else {
                    0
                },
                entry.base_model(),
                self.active != Some(entry.id()),
                !self.store().state(entry).is_usable(),
                self.store().state(entry).partial().is_none(),
                entry.quant != "Q4_K_M",
            )
        });
        for entry in entries.into_iter().filter(|_| !folder_missing) {
            let base = entry.base_model();
            if !self.details
                && entry.is_plamo()
                && !matches!(entry.size, "2b" | "8b")
                && !installed_groups.contains(&base)
                && self.store().state(entry).partial().is_none()
                && !self.progress.contains_key(entry.id())
            {
                continue;
            }
            if base == current_base && !self.details {
                continue;
            }
            if base != current_base {
                if self.details {
                    rows.push(size_header(entry, muted, divider));
                }
                current_base = base;
            }
            rows.push(self.render_row(entry, compact_details, cx));
        }
        let catalog_text = match &self.catalog_status {
            CatalogStatus::Checking => t!("models.catalog_checking").into_owned(),
            CatalogStatus::UpToDate => t!("models.catalog_up_to_date").into_owned(),
            CatalogStatus::Updated(n) => t!("models.catalog_updated", count = n).into_owned(),
            CatalogStatus::Failed(e) => t!("models.catalog_failed", message = e).into_owned(),
        };
        let models_dir = self.store().dir().to_path_buf();
        let custom_folder = models_dir != crate::settings::default_models_dir();
        let downloading = !self.progress.is_empty();
        let danger = cx.theme().danger;

        v_flex()
            .gap_3()
            .text_sm()
            .when(folder_missing, |panel| {
                panel
                    .child(
                        div()
                            .text_color(danger)
                            .child(t!("models.folder_missing").into_owned()),
                    )
                    .child(
                        Button::new("select-missing-folder")
                            .self_start()
                            .label(t!("models.select_folder").into_owned())
                            .disabled(downloading)
                            .on_click(cx.listener(|this, _, _, cx| this.choose_folder(cx))),
                    )
            })
            .when(self.details, |panel| {
                panel.child(
                    v_flex()
                        .gap_1()
                        .items_start()
                        .text_xs()
                        .text_color(muted)
                        .child(
                            div()
                                .min_w_0()
                                .max_w_full()
                                .child(t!("models.quality_legend").into_owned()),
                        )
                        .child(
                            Button::new("quality-source")
                                .link()
                                .xsmall()
                                .label(t!("models.quality_source").into_owned())
                                .on_click(|_, _, cx| cx.open_url(QUALITY_SOURCE_URL)),
                        ),
                )
            })
            .child(
                v_flex()
                    .gap_0p5()
                    .max_h(scaled(440.))
                    .overflow_y_scrollbar()
                    .id("model-list")
                    .children(rows),
            )
            .child(plamo_license_button().self_start())
            .child(
                Button::new("model-details")
                    .link()
                    .xsmall()
                    .label(t!("models.details").into_owned())
                    .on_click(cx.listener(|this, _, _, cx| {
                        this.details = !this.details;
                        cx.notify();
                    })),
            )
            .when(self.details, |panel| {
                panel.child(
                    h_flex()
                        .flex_wrap()
                        .gap_2()
                        .text_xs()
                        .text_color(muted)
                        .child(
                            t!("models.folder", path = models_dir.display().to_string())
                                .into_owned(),
                        )
                        .child(
                            Button::new("open-folder")
                                .ghost()
                                .xsmall()
                                .label(t!("models.open_folder").into_owned())
                                .disabled(folder_missing)
                                .on_click(cx.listener(|this, _, _, cx| {
                                    if !this.folder_missing() {
                                        let dir = this.store().dir();
                                        let _ = std::fs::create_dir_all(dir);
                                        cx.reveal_path(dir);
                                    }
                                })),
                        )
                        .child(
                            Button::new("change-folder")
                                .ghost()
                                .xsmall()
                                .label(t!("models.change_folder").into_owned())
                                .tooltip(t!("models.change_folder_note").into_owned())
                                // Running downloads would finish in the old folder.
                                .disabled(downloading)
                                .on_click(cx.listener(|this, _, _, cx| this.choose_folder(cx))),
                        )
                        .when(custom_folder, |this| {
                            this.child(
                                Button::new("default-folder")
                                    .ghost()
                                    .xsmall()
                                    .label(t!("models.default_folder").into_owned())
                                    .disabled(downloading)
                                    .on_click(cx.listener(|_, _, _, cx| {
                                        cx.emit(ModelsEvent::ChangeFolder(None))
                                    })),
                            )
                        })
                        .child(div().flex_1())
                        .child(catalog_text)
                        .when(
                            !matches!(self.catalog_status, CatalogStatus::Checking),
                            |this| {
                                this.child(
                                    Button::new("refresh-catalog")
                                        .ghost()
                                        .xsmall()
                                        .label(t!("models.catalog_refresh").into_owned())
                                        .on_click(cx.listener(|this, _, _, cx| {
                                            this.refresh_catalog(cx);
                                            cx.notify();
                                        })),
                                )
                            },
                        ),
                )
            })
            .when_some(self.folder_error.clone(), |this, error| {
                this.child(
                    div()
                        .text_xs()
                        .text_color(danger)
                        .child(t!("models.folder_failed", message = error).into_owned()),
                )
            })
    }
}

/// Header above the files of one model size: description and the unquantized quality.
fn size_header(entry: &CatalogEntry, muted: Hsla, divider: Hsla) -> AnyElement {
    if let Some(name) = entry.name {
        return h_flex()
            .mt_2()
            .px_2()
            .pb_1()
            .gap_3()
            .border_b_1()
            .border_color(divider)
            .child(
                div()
                    .flex_shrink_0()
                    .font_weight(FontWeight::SEMIBOLD)
                    .child(name),
            )
            .child(
                div()
                    .flex_1()
                    .min_w_0()
                    .text_xs()
                    .text_color(muted)
                    .child(verified_pairs(entry)),
            )
            .child(model_license_button(entry))
            .into_any_element();
    }
    let description = match entry.size {
        "0.8b" => t!("models.size_08b"),
        "2b" => t!("models.size_2b"),
        "8b" => t!("models.size_8b"),
        "31b" => t!("models.size_31b"),
        _ => t!("models.size_other"),
    };
    h_flex()
        .mt_2()
        .px_2()
        .pb_1()
        .gap_3()
        .border_b_1()
        .border_color(divider)
        .child(
            div()
                .flex_shrink_0()
                .font_weight(FontWeight::SEMIBOLD)
                .child(entry.size_label()),
        )
        .child(
            div()
                .flex_1()
                .min_w_0()
                .text_xs()
                .text_color(muted)
                .child(description.into_owned()),
        )
        .when_some(entry.reference_quality(), |this, r| {
            this.child(
                div().text_xs().text_color(muted).child(
                    t!(
                        "models.bf16_reference",
                        ja = lang::format_decimal(format!("{:.3}", r.pftb)),
                        multi = lang::format_decimal(format!("{:.1}", r.flores_ja))
                    )
                    .into_owned(),
                ),
            )
        })
        .into_any_element()
}

pub fn verified_pairs(entry: &CatalogEntry) -> String {
    let caps = entry.profile.capabilities();
    let Some(&[(source, _), ..]) = caps.directions else {
        return String::new();
    };
    let targets = caps
        .targets(Some(source))
        .map(lang::language_name)
        .collect::<Vec<_>>()
        .join(&t!("models.pair_separator"));
    t!(
        "models.verified_pairs",
        source = lang::language_name(source),
        targets = targets
    )
    .into_owned()
}

pub fn plamo_license_button() -> Button {
    Button::new("read-plamo-license")
        .link()
        .xsmall()
        .label(t!("models.license_link").into_owned())
        .on_click(|_, _, cx| cx.open_url(&license_url()))
}

pub fn model_license_button(entry: &CatalogEntry) -> Button {
    let (label, url) = match entry.profile.capabilities().license {
        ModelLicense::PlamoCommunity => (t!("models.license_name").into_owned(), license_url()),
        ModelLicense::Apache2 => ("Apache-2.0".to_owned(), entry.model_card_url()),
    };
    Button::new(SharedString::from(format!("license-{}", entry.id())))
        .link()
        .xsmall()
        .label(label)
        .on_click(move |_, _, cx| cx.open_url(&url))
}

fn quant_note(quant: &str) -> Option<String> {
    let key = match quant {
        "Q4_K_M" => "models.quant_q4km",
        "Q4_0" => "models.quant_q40",
        "Q3_K_M" => "models.quant_q3km",
        "Q2_K" => "models.quant_q2k",
        "Q8_0" => "models.quant_q80",
        _ => return None,
    };
    Some(t!(key).into_owned())
}

/// Published quality of a file, with the difference from the unquantized model.
/// Larger drops are highlighted: thresholds roughly separate "same as BF16" from noticeable loss.
fn quality_cell(entry: &CatalogEntry, muted: Hsla, warning: Hsla, danger: Hsla) -> AnyElement {
    let (Some(q), Some(r)) = (entry.quality(), entry.reference_quality()) else {
        return div()
            .w(scaled(170.))
            .flex_shrink_0()
            .text_xs()
            .text_color(muted)
            .child(t!("models.quality_unknown").into_owned())
            .into_any_element();
    };
    let line = |label: String, value: String, delta: f32, warn: f32, bad: f32, digits: usize| {
        let color = if delta <= -bad {
            danger
        } else if delta <= -warn {
            warning
        } else {
            muted
        };
        h_flex()
            .gap_1()
            .child(div().text_color(muted).child(label))
            .child(div().child(value))
            .child(div().text_color(color).child(format!(
                "({})",
                lang::format_decimal(format!("{delta:+.digits$}"))
            )))
    };
    v_flex()
        .w(scaled(170.))
        .flex_shrink_0()
        .text_xs()
        .child(line(
            t!("models.metric_ja").into_owned(),
            lang::format_decimal(format!("{:.3}", q.pftb)),
            q.pftb - r.pftb,
            0.003,
            0.008,
            3,
        ))
        .child(line(
            t!("models.metric_multi").into_owned(),
            lang::format_decimal(format!("{:.1}", q.flores_ja)),
            q.flores_ja - r.flores_ja,
            0.3,
            0.8,
            1,
        ))
        .into_any_element()
}

pub fn progress_text(p: &Progress) -> String {
    let mut text = format!(
        "{:.0}% · {} / {}",
        p.fraction() * 100.0,
        format_bytes(p.downloaded),
        format_bytes(p.total)
    );
    if p.bytes_per_second > 0.0 {
        text.push_str(&format!(" · {}/s", format_bytes(p.bytes_per_second as u64)));
    }
    if let Some(eta) = p.eta() {
        text.push_str(&format!(" · {}", crate::lang::format_duration(eta)));
    }
    text
}

pub fn format_bytes(bytes: u64) -> String {
    format_bytes_for_locale(bytes, &rust_i18n::locale())
}

fn format_bytes_for_locale(bytes: u64, locale: &str) -> String {
    const MB: f64 = 1e6;
    const GB: f64 = 1e9;
    let b = bytes as f64;
    if b >= GB {
        let size = lang::decimal_for_locale(format!("{:.2}", b / GB), locale);
        t!("size.gb", locale = locale, size = size).into_owned()
    } else {
        t!("size.mb", locale = locale, size = format!("{:.0}", b / MB)).into_owned()
    }
}

pub(crate) fn hy_mt2_7b_memory_warning(system_memory: u64, model_bytes: u64) -> bool {
    system_memory > 0 && crate::backend::estimate_required(model_bytes) > system_memory / 2
}

fn model_support(entry: &CatalogEntry, locale: &str) -> String {
    let key = match entry.profile {
        ModelProfile::PlamoPfml => {
            return t!(
                "models.plamo_support",
                locale = locale,
                count = engine::pfml::LANGUAGES.len()
            )
            .into_owned();
        }
        ModelProfile::HyMt2 if entry.capabilities().files.contains(&FileFormat::PowerPoint) => {
            "models.hy_office_pdf_support"
        }
        ModelProfile::HyMt2 => "models.hy_support",
        ModelProfile::IndexTranslate => "models.index_support",
    };
    let count = entry.capabilities().targets(Some("en")).count();
    t!(key, locale = locale, count = count).into_owned()
}

#[cfg(test)]
mod tests {
    use super::{catalog, format_bytes_for_locale, hy_mt2_7b_memory_warning, model_support};
    use gpui::AppContext as _;

    #[test]
    fn model_rows_show_language_counts_in_both_locales() {
        for (entry, locale, expected) in [
            (&catalog::HY_MT2, "en", "EN ↔ 9 · TXT, MD, Office, PDF"),
            (
                &catalog::HY_MT2,
                "ja",
                "英語 ↔ 9言語 · TXT、MD、Office、PDF",
            ),
            (&catalog::HY_MT2_7B, "en", "EN ↔ 9 · TXT, MD, Office, PDF"),
            (
                &catalog::HY_MT2_7B,
                "ja",
                "英語 ↔ 9言語 · TXT、MD、Office、PDF",
            ),
            (
                &catalog::INDEX_TRANSLATE,
                "en",
                "English ↔ 6 languages · TXT, Markdown, Word",
            ),
            (
                &catalog::INDEX_TRANSLATE,
                "ja",
                "英語 ↔ 6言語 · TXT、Markdown、Word",
            ),
        ] {
            assert_eq!(model_support(entry, locale), expected);
        }
    }

    #[gpui::test]
    fn next_routing_request_skips_a_recorded_failure(cx: &mut gpui::TestAppContext) {
        use super::{ModelStore, ModelsPanel};
        use engine::selection::{QualityTier, Request};
        use std::time::{Duration, SystemTime, UNIX_EPOCH};

        let nonce = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let dir = std::env::temp_dir().join(format!(
            "plamo-failed-file-test-{}-{nonce}",
            std::process::id()
        ));
        std::fs::create_dir(&dir).unwrap();
        let store = ModelStore::new(&dir);
        let hy = &catalog::HY_MT2;
        let plamo = catalog::entries()
            .into_iter()
            .find(|entry| entry.is_plamo() && entry.size == "2b")
            .unwrap();
        for entry in [hy, plamo] {
            std::fs::File::create(store.path(entry))
                .unwrap()
                .set_len(entry.bytes)
                .unwrap();
        }
        let entries = [hy, plamo];
        let request = Request {
            source: Some("en"),
            target: Some("fr"),
            format: None,
            system_memory: 0,
            quality: QualityTier::Standard,
            plamo_consent: true,
            bidirectional: false,
            selected: None,
        };
        let path = store.path(hy);
        cx.update(|cx| {
            let panel = cx.new(|cx| {
                let mut panel = ModelsPanel::new(store, None, true, dir.join("catalog.json"), cx);
                panel._refresh = gpui::Task::ready(());
                panel
            });
            let route = |cx: &gpui::App| {
                panel
                    .read(cx)
                    .select_automatically(request, &entries)
                    .usable
            };
            assert_eq!(route(cx), Some(hy));
            panel.update(cx, |panel, cx| {
                panel.record_load_failure(hy, "broken file".into(), true, cx);
            });
            assert_eq!(route(cx), Some(plamo));
            std::fs::OpenOptions::new()
                .write(true)
                .open(&path)
                .unwrap()
                .set_times(
                    std::fs::FileTimes::new().set_modified(UNIX_EPOCH + Duration::from_secs(1)),
                )
                .unwrap();
            assert_eq!(route(cx), Some(hy));
        });
        std::fs::remove_dir_all(dir).unwrap();
    }

    #[gpui::test]
    fn alternative_download_ends_setup_but_partial_and_deleted_files_do_not(
        cx: &mut gpui::TestAppContext,
    ) {
        use super::{ModelStore, ModelsPanel, setup};
        use std::fs::{self, File};

        let dir = std::env::temp_dir().join(format!("translate-setup-test-{}", std::process::id()));
        fs::create_dir(&dir).unwrap();
        let store = ModelStore::new(&dir);
        let index = &catalog::INDEX_TRANSLATE;
        let path = store.path(index);
        let partial = store.partial_path(index);
        File::create(&partial).unwrap().set_len(100).unwrap();
        cx.update(|cx| {
            let panel = cx.new(|cx| {
                let mut panel = ModelsPanel::new(store, None, false, dir.join("catalog.json"), cx);
                panel._refresh = gpui::Task::ready(());
                panel
            });
            let models = panel.read(cx);
            assert!(setup::show_card(models.has_usable_model(None), false));
            assert!(!models.use_downloaded_for_setup(index, true));

            // Simulate the downloader promoting a verified Index-Translate file into place.
            File::options()
                .write(true)
                .open(&partial)
                .unwrap()
                .set_len(index.bytes)
                .unwrap();
            fs::rename(partial, &path).unwrap();
            assert!(models.use_downloaded_for_setup(index, true));
            assert!(!setup::show_card(models.has_usable_model(None), false));

            fs::remove_file(path).unwrap();
            assert!(setup::show_card(models.has_usable_model(None), false));
        });
        fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn formatting() {
        assert_eq!(format_bytes_for_locale(1_807_192_480, "en"), "1.81 GB");
        assert_eq!(format_bytes_for_locale(573_116_288, "en"), "573 MB");
        assert_eq!(format_bytes_for_locale(1_807_192_480, "fr"), "1,81 Go");
        assert_eq!(format_bytes_for_locale(573_116_288, "fr"), "573 Mo");
        assert_eq!(format_bytes_for_locale(1_807_192_480, "pt-BR"), "1,81 GB");
    }
    #[test]
    fn warns_when_the_estimate_exceeds_half_of_known_system_memory() {
        let bytes = catalog::HY_MT2_7B.bytes;
        let gib = 1 << 30;
        assert!(!hy_mt2_7b_memory_warning(0, bytes));
        assert!(hy_mt2_7b_memory_warning(8 * gib, bytes));
        assert!(!hy_mt2_7b_memory_warning(16 * gib, bytes));
        let boundary = 2 * crate::backend::estimate_required(bytes);
        assert!(!hy_mt2_7b_memory_warning(boundary, bytes));
    }
}
