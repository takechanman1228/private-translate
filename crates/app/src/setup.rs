//! Choices and recovery for the first model, before there is source text to detect.

use std::io;
use std::path::Path;

use engine::download::DownloadError;
use engine::models::{CatalogEntry, HY_MT2, HY_MT2_7B, INDEX_TRANSLATE, ModelStore};
use engine::profiles::FileFormat;
use engine::selection::{self, QualityTier};
use engine::worker::LoadFailure;

pub fn installed_memory() -> u64 {
    let mut system = sysinfo::System::new();
    system.refresh_memory_specifics(sysinfo::MemoryRefreshKind::new().with_ram());
    system.total_memory()
}

pub fn show_card(has_usable_model: bool, loading: bool) -> bool {
    !has_usable_model && !loading
}

#[derive(Clone, Copy)]
pub struct Request<'a> {
    pub target: &'a str,
    pub format: Option<FileFormat>,
    pub memory: u64,
    pub quality: QualityTier,
    pub consent: bool,
}

pub fn recommend(
    request: Request<'_>,
    entries: &[&'static CatalogEntry],
    mut eligible: impl FnMut(&CatalogEntry) -> bool,
) -> Option<&'static CatalogEntry> {
    let document = request.format.is_some_and(FileFormat::is_document);
    let fits_7b = selection::fits_memory_budget(HY_MT2_7B.bytes, request.memory);
    let candidates: Vec<_> = entries
        .iter()
        .copied()
        .filter(|entry| eligible(entry))
        // Preserve the setup download choices for newly enabled Index formats.
        .filter(|entry| {
            entry.id() != INDEX_TRANSLATE.id()
                || matches!(request.format, None | Some(FileFormat::Txt))
        })
        .filter(|entry| {
            request
                .format
                .is_none_or(|format| entry.capabilities().files.contains(&format))
                && (entry.id() != HY_MT2_7B.id() || fits_7b)
                && (!document
                    || entry.id() != HY_MT2.id()
                    || !fits_7b && selection::fits_memory_budget(entry.bytes, request.memory))
        })
        .collect();
    let plamo = || {
        selection::select(
            selection::Request {
                source: None,
                target: Some(request.target),
                format: request.format,
                system_memory: request.memory,
                quality: request.quality,
                plamo_consent: request.consent,
                bidirectional: false,
                selected: None,
            },
            &candidates,
            &[],
        )
        .suggestion
        .map(|suggestion| suggestion.entry)
    };
    // Keep PLaMo as the document recommendation once consent is recorded.
    if document
        && request.consent
        && let Some(entry) = plamo()
    {
        return Some(entry);
    }
    // 7B's Detect rule ranks Apache-licensed models by coverage before we have any input.
    candidates
        .iter()
        .copied()
        .filter_map(|entry| {
            let sources = entry
                .capabilities()
                .directions?
                .iter()
                .filter(|&&(_, to)| to == request.target)
                .count();
            (sources > 0).then_some((entry, sources))
        })
        .max_by_key(|(entry, sources)| {
            (
                *sources,
                request.quality == QualityTier::High && entry.id() == HY_MT2_7B.id(),
                std::cmp::Reverse(entry.bytes),
            )
        })
        .map(|(entry, _)| entry)
        .or_else(plamo)
}

pub fn smaller(
    request: Request<'_>,
    entries: &[&'static CatalogEntry],
    original: &CatalogEntry,
    mut fits_disk: impl FnMut(&CatalogEntry) -> bool,
) -> Option<&'static CatalogEntry> {
    let mut eligible = |entry: &CatalogEntry| {
        entry.bytes < original.bytes
            && !(entry.is_plamo() && original.is_plamo() && entry.size == original.size)
            && fits_disk(entry)
    };
    recommend(request, entries, &mut eligible).or_else(|| {
        // Smaller PLaMo weights are already in Models. Keep them out of the default
        // recommendation, but offer them if no model in the normal order fits.
        entries
            .iter()
            .copied()
            .filter(|entry| {
                entry.is_plamo()
                    && eligible(entry)
                    && request
                        .format
                        .is_none_or(|format| entry.capabilities().files.contains(&format))
                    && selection::supports_direction(entry.profile, None, Some(request.target))
                    && (entry.params_billion() < 8.0
                        || selection::fits_memory_budget(entry.bytes, request.memory))
            })
            .max_by_key(|entry| (entry.params_billion().to_bits(), entry.bytes))
    })
}

/// An absent default directory belongs to a fresh profile and can be created on download.
pub fn configured_folder_missing(configured: Option<&Path>) -> bool {
    configured.is_some_and(|dir| !dir.is_dir())
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Alternative {
    Installed,
    Download,
}

pub fn load_alternative(
    failure: LoadFailure,
    request: Request<'_>,
    entries: &[&'static CatalogEntry],
    failed: &CatalogEntry,
    mut availability: impl FnMut(&CatalogEntry) -> Option<Alternative>,
) -> Option<(&'static CatalogEntry, Alternative)> {
    if failure != LoadFailure::OutOfMemory {
        return None;
    }
    for action in [Alternative::Installed, Alternative::Download] {
        if let Some(entry) = smaller(request, entries, failed, |entry| {
            selection::fits_memory_budget(entry.bytes, request.memory)
                && availability(entry) == Some(action)
        }) {
            return Some((entry, action));
        }
    }
    None
}

pub fn offers_more_file_formats(text: &CatalogEntry, files: &CatalogEntry) -> bool {
    let text_formats = text.capabilities().files;
    files
        .capabilities()
        .files
        .iter()
        .any(|format| !text_formats.contains(format))
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Failure {
    Disk { needed: u64 },
    Storage,
    Download,
    Verification,
    Load,
}

impl Failure {
    pub fn from_download(error: &DownloadError) -> Self {
        match error {
            DownloadError::ChecksumMismatch { .. } | DownloadError::SizeMismatch { .. } => {
                Self::Verification
            }
            DownloadError::Io(_) => Self::Storage,
            _ => Self::Download,
        }
    }

    pub fn message(self, has_smaller: bool) -> &'static str {
        match (self, has_smaller) {
            (Self::Disk { .. }, true) => "setup.disk_shortage_smaller",
            (Self::Disk { .. }, false) => "setup.disk_shortage",
            (Self::Storage, _) => "setup.storage_error",
            (Self::Download, _) => "setup.download_error",
            (Self::Verification, _) => "setup.verify_error",
            (Self::Load, true) => "setup.load_error_smaller",
            (Self::Load, false) => "setup.load_error",
        }
    }

    pub fn action(self, has_smaller: bool) -> &'static str {
        match (self, has_smaller) {
            (Self::Disk { .. } | Self::Load, true) => "setup.use_smaller",
            (Self::Verification, _) => "setup.download_again",
            _ => "models.retry",
        }
    }

    pub fn offers_smaller(self) -> bool {
        matches!(self, Self::Disk { .. } | Self::Load)
    }
}

fn remaining(bytes: u64, partial: Option<u64>) -> u64 {
    // The downloader restarts if the partial file is bigger than the expected download.
    bytes - partial.filter(|&n| n <= bytes).unwrap_or_default()
}

// Leave 1 GB for the OS and other writes while the model is downloading.
const SPACE_RESERVE: u64 = 1_000_000_000;

pub fn check_space(bytes: u64, partial: Option<u64>, free: Option<u64>) -> Result<(), Failure> {
    let remaining = remaining(bytes, partial);
    let needed = remaining.saturating_add(SPACE_RESERVE);
    match free {
        _ if remaining == 0 => Ok(()),
        Some(available) if available >= needed => Ok(()),
        Some(_) => Err(Failure::Disk { needed }),
        None => Err(Failure::Storage),
    }
}

pub fn blocks_download(result: Result<(), Failure>) -> bool {
    matches!(result, Err(Failure::Disk { .. }))
}

pub fn preflight(
    store: &ModelStore,
    entry: &CatalogEntry,
    free: Option<u64>,
) -> Result<(), Failure> {
    check_space(entry.bytes, store.state(entry).partial(), free)
}

pub fn available_space(dir: &Path) -> io::Result<u64> {
    // A fresh profile may not have a models directory yet. Check the filesystem where
    // it will be created, including when the user selected another volume or a symlink.
    let absolute = std::path::absolute(dir)?;
    for parent in absolute.ancestors() {
        match parent.metadata() {
            Ok(metadata) if metadata.is_dir() => return volume_available(parent),
            Err(error) if error.kind() == io::ErrorKind::NotFound => {}
            Err(error) => return Err(error),
            Ok(_) => return Err(io::ErrorKind::NotADirectory.into()),
        }
    }
    Err(io::ErrorKind::NotFound.into())
}

#[cfg(unix)]
fn volume_available(dir: &Path) -> io::Result<u64> {
    #[cfg(target_os = "macos")]
    if let Some(capacity) = desktop::volume_capacity_for_important_usage(dir) {
        return Ok(capacity);
    }
    let stats = rustix::fs::statvfs(dir)?;
    Ok(stats.f_bavail.saturating_mul(stats.f_frsize))
}

#[cfg(windows)]
fn volume_available(dir: &Path) -> io::Result<u64> {
    use std::os::windows::ffi::OsStrExt;
    use windows_sys::Win32::Storage::FileSystem::GetDiskFreeSpaceExW;

    let path: Vec<u16> = dir.as_os_str().encode_wide().chain([0]).collect();
    let mut free = 0;
    // SAFETY: path is NUL-terminated, free is writable, and both other outputs are optional.
    let result = unsafe {
        GetDiskFreeSpaceExW(
            path.as_ptr(),
            &mut free,
            std::ptr::null_mut(),
            std::ptr::null_mut(),
        )
    };
    if result == 0 {
        Err(io::Error::last_os_error())
    } else {
        Ok(free)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use engine::models::{self as catalog, HY_MT2, INDEX_TRANSLATE};

    const GIB: u64 = 1 << 30;

    fn request(target: &str, memory: u64, format: Option<FileFormat>) -> Request<'_> {
        Request {
            target,
            format,
            memory,
            quality: QualityTier::Standard,
            consent: false,
        }
    }

    #[test]
    fn only_a_configured_unavailable_directory_requires_folder_recovery() {
        let dir = std::env::temp_dir();
        let missing = dir.join(format!("missing-model-folder-{}", std::process::id()));
        let file = std::env::current_exe().unwrap();

        assert!(!configured_folder_missing(None));
        assert!(!configured_folder_missing(Some(&dir)));
        assert!(configured_folder_missing(Some(&missing)));
        assert!(configured_folder_missing(Some(&file)));
    }

    #[test]
    fn load_recovery_offers_smaller_weights_only_for_memory_and_prefers_installed() {
        let plamo = catalog::default_entry();
        let entries = [&HY_MT2, &HY_MT2_7B, plamo];
        let request = request("ja", 16 * GIB, None);
        let availability = |entry: &CatalogEntry| {
            if entry == plamo {
                Some(Alternative::Installed)
            } else {
                Some(Alternative::Download)
            }
        };
        for failure in [LoadFailure::InvalidFile, LoadFailure::Other] {
            assert!(
                load_alternative(failure, request, &entries, &HY_MT2_7B, availability).is_none()
            );
        }
        let oom = LoadFailure::OutOfMemory;
        assert_eq!(
            load_alternative(oom, request, &entries, &HY_MT2_7B, availability),
            Some((plamo, Alternative::Installed)),
        );
        assert_eq!(
            load_alternative(oom, request, &entries, &HY_MT2_7B, |entry| {
                (entry == &HY_MT2).then_some(Alternative::Download)
            }),
            Some((&HY_MT2, Alternative::Download)),
        );
        assert!(load_alternative(oom, request, &entries, &HY_MT2, availability).is_none(),);
        assert!(
            load_alternative(
                oom,
                Request {
                    memory: GIB,
                    ..request
                },
                &entries,
                &HY_MT2_7B,
                availability
            )
            .is_none(),
        );
    }

    #[test]
    fn detects_into_both_os_languages_with_memory_and_format() {
        for target in ["en", "ja"] {
            for gib in [8, 16, 32] {
                let entries = catalog::entries();
                let text = recommend(request(target, gib * GIB, None), &entries, |_| true).unwrap();
                assert_eq!(text, &HY_MT2);
                assert_eq!(text.bytes, HY_MT2.bytes);
                assert!(!text.is_plamo());
                for format in [FileFormat::Word, FileFormat::PowerPoint, FileFormat::Pdf] {
                    let files =
                        recommend(request(target, gib * GIB, Some(format)), &entries, |_| true)
                            .unwrap();
                    assert_eq!(files, if gib == 8 { &HY_MT2 } else { &HY_MT2_7B });
                }
            }
        }
    }

    #[test]
    fn applies_coverage_consent_and_quality_rules_for_detect() {
        let entries = catalog::entries();
        let pick = |request| recommend(request, &entries, |_| true);
        let mut high = request("en", 8 * GIB, None);
        high.quality = QualityTier::High;
        assert_eq!(pick(high), Some(&HY_MT2));
        high.memory = 16 * GIB;
        assert_eq!(pick(high), Some(&HY_MT2_7B));
        let portuguese = request("pt", 8 * GIB, Some(FileFormat::Pdf));
        assert_eq!(pick(portuguese), Some(&HY_MT2));
        let traditional = request("zh-hant", 16 * GIB, None);
        assert_eq!(pick(traditional), Some(catalog::default_entry()));
        let mut documents = request("en", 16 * GIB, Some(FileFormat::Pdf));
        documents.consent = true;
        assert_eq!(pick(documents), Some(catalog::default_entry()));
        assert!(offers_more_file_formats(&INDEX_TRANSLATE, &HY_MT2));
        assert!(!offers_more_file_formats(&HY_MT2, &INDEX_TRANSLATE));
        assert!(!offers_more_file_formats(&HY_MT2, &HY_MT2));
        assert!(!offers_more_file_formats(&HY_MT2, &HY_MT2_7B));
        assert!(!offers_more_file_formats(&HY_MT2, catalog::default_entry()));
        assert!(!offers_more_file_formats(&HY_MT2_7B, &HY_MT2));
    }

    #[test]
    fn first_run_files_recommends_a_model_for_the_selected_format() {
        let entries = catalog::entries();
        for (memory, consent, format, entry) in [
            (8 * GIB, false, FileFormat::Pdf, &HY_MT2),
            (8 * GIB, false, FileFormat::Word, &HY_MT2),
            (8 * GIB, false, FileFormat::Markdown, &HY_MT2),
            (8 * GIB, false, FileFormat::Txt, &HY_MT2),
            (8 * GIB, false, FileFormat::PowerPoint, &HY_MT2),
            (8 * GIB, true, FileFormat::Word, catalog::default_entry()),
            (
                8 * GIB,
                true,
                FileFormat::PowerPoint,
                catalog::default_entry(),
            ),
            (16 * GIB, false, FileFormat::PowerPoint, &HY_MT2_7B),
            (
                16 * GIB,
                true,
                FileFormat::PowerPoint,
                catalog::default_entry(),
            ),
        ] {
            assert_eq!(
                recommend(
                    Request {
                        consent,
                        ..request("ja", memory, Some(format))
                    },
                    &entries,
                    |_| true
                ),
                Some(entry)
            );
        }
        let large_mac = request("ja", 16 * GIB, Some(FileFormat::Pdf));
        assert_eq!(
            recommend(large_mac, &entries, |entry| entry != &HY_MT2_7B),
            Some(catalog::default_entry())
        );
        for format in [
            FileFormat::Markdown,
            FileFormat::Pdf,
            FileFormat::Word,
            FileFormat::PowerPoint,
        ] {
            assert_eq!(
                recommend(request("ja", 8 * GIB, Some(format)), &entries, |entry| {
                    entry == &INDEX_TRANSLATE
                }),
                None
            );
        }
    }

    #[test]
    fn reserves_space_beyond_the_remaining_download() {
        assert_eq!(
            check_space(100, None, Some(100)),
            Err(Failure::Disk {
                needed: SPACE_RESERVE + 100
            })
        );
        assert_eq!(
            check_space(100, Some(40), Some(SPACE_RESERVE + 59)),
            Err(Failure::Disk {
                needed: SPACE_RESERVE + 60
            })
        );
        assert_eq!(check_space(100, Some(40), Some(SPACE_RESERVE + 60)), Ok(()));
        assert_eq!(check_space(100, Some(100), Some(0)), Ok(()));
        assert_eq!(check_space(100, Some(100), None), Ok(()));
        assert_eq!(
            check_space(100, Some(101), Some(0)),
            Err(Failure::Disk {
                needed: SPACE_RESERVE + 100
            })
        );
        assert!(blocks_download(check_space(100, None, Some(100))));
        let unknown = check_space(100, None, None);
        assert_eq!(unknown, Err(Failure::Storage));
        assert!(!blocks_download(unknown));
    }

    #[test]
    fn finds_a_compatible_smaller_model_with_the_disk_reserve() {
        let entries = catalog::entries();
        let req = request("ja", 16 * GIB, Some(FileFormat::PowerPoint));
        let fits = |free| {
            smaller(req, &entries, &HY_MT2_7B, |entry| {
                check_space(entry.bytes, None, Some(free + SPACE_RESERVE)).is_ok()
            })
        };
        assert_eq!(
            fits(catalog::default_entry().bytes),
            Some(catalog::default_entry())
        );
        assert_eq!(fits(600_000_000).unwrap().size, "0.8b");
        assert!(fits(400_000_000).is_none());
        assert_eq!(
            smaller(request("en", 8 * GIB, None), &entries, &HY_MT2, |_| true)
                .unwrap()
                .size,
            "0.8b"
        );
        assert!(available_space(&std::env::temp_dir().join("missing-model-dir")).is_ok());
    }

    #[test]
    fn has_a_specific_action_after_each_failure() {
        for (failure, message, smaller_message, action) in [
            (
                Failure::Disk { needed: 10 },
                "setup.disk_shortage",
                "setup.disk_shortage_smaller",
                "setup.use_smaller",
            ),
            (
                Failure::Storage,
                "setup.storage_error",
                "setup.storage_error",
                "models.retry",
            ),
            (
                Failure::from_download(&DownloadError::Status(503)),
                "setup.download_error",
                "setup.download_error",
                "models.retry",
            ),
            (
                Failure::from_download(&DownloadError::Incomplete {
                    downloaded: 5,
                    total: 10,
                }),
                "setup.download_error",
                "setup.download_error",
                "models.retry",
            ),
            (
                Failure::from_download(&DownloadError::ChecksumMismatch {
                    expected: "a".into(),
                    actual: "b".into(),
                }),
                "setup.verify_error",
                "setup.verify_error",
                "setup.download_again",
            ),
            (
                Failure::Load,
                "setup.load_error",
                "setup.load_error_smaller",
                "setup.use_smaller",
            ),
        ] {
            assert_eq!(failure.message(false), message);
            assert_eq!(failure.message(true), smaller_message);
            assert_eq!(failure.action(true), action);
            assert_eq!(
                failure.action(false),
                if failure == Failure::Verification {
                    "setup.download_again"
                } else {
                    "models.retry"
                }
            );
        }
    }

    #[test]
    fn hides_the_card_when_a_model_is_usable_or_loading() {
        assert!(show_card(false, false));
        assert!(!show_card(false, true));
        assert!(!show_card(true, false));
        assert!(!show_card(true, true));
    }

    #[test]
    fn disk_and_load_message_variants_are_localized() {
        use rust_i18n::t;

        let locales = [
            "en", "ja", "zh-Hans", "zh-Hant", "ko", "es", "fr", "de", "pt-BR", "id",
        ];
        for key in [
            "models.folder_missing",
            "models.select_folder",
            "models.load_failed",
            "models.load_memory",
            "models.use_named",
            "models.download_named",
            "models.agree_download_named",
        ] {
            for locale in locales {
                let message = t!(key, locale = locale, model = "Synthetic 1B");
                assert_ne!(message, key);
                if locale != "en" {
                    assert_ne!(message, t!(key, locale = "en", model = "Synthetic 1B"));
                }
            }
        }

        for failure in [Failure::Disk { needed: 10 }, Failure::Load] {
            let retry = failure.message(false);
            let smaller = failure.message(true);
            for locale in locales {
                let retry_text = t!(retry, locale = locale, size = "10 GB");
                let smaller_text = t!(smaller, locale = locale, size = "10 GB");
                assert_ne!(retry_text, retry);
                assert_ne!(smaller_text, smaller);
                assert_ne!(retry_text, smaller_text);
                if locale != "en" {
                    assert_ne!(retry_text, t!(retry, locale = "en", size = "10 GB"));
                    assert_ne!(smaller_text, t!(smaller, locale = "en", size = "10 GB"));
                }
            }
        }
    }
}
