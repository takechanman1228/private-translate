use std::ffi::{OsStr, OsString};
use std::path::{Path, PathBuf};

use unicode_segmentation::UnicodeSegmentation;

use crate::FileError;

#[derive(Clone, Copy)]
pub(super) enum NameRules {
    Unix,
    // Length of the absolute directory path INCLUDING its final separator.
    Windows { prefix_units: usize },
}

impl NameRules {
    pub(super) fn for_destination(path: &Path) -> Result<Self, FileError> {
        if cfg!(windows) {
            // A one-unit placeholder handles relative paths, drive roots and UNC shares
            // without guessing whether the directory already ends in a separator.
            let probe =
                std::path::absolute(path.with_file_name("x")).map_err(crate::destination_error)?;
            Ok(Self::Windows {
                prefix_units: native_units(probe.as_os_str()) - 1,
            })
        } else {
            Ok(Self::Unix)
        }
    }

    pub(super) fn capacity(self) -> usize {
        match self {
            Self::Unix => 255,
            // Traditional MAX_PATH includes the terminating NUL.
            Self::Windows { prefix_units } => 255.min(259usize.saturating_sub(prefix_units)),
        }
    }

    fn units(self, text: &str) -> usize {
        match self {
            Self::Unix => text.len(),
            Self::Windows { .. } => text.encode_utf16().count(),
        }
    }

    fn fit_stem(self, stem: &str, suffix_units: usize) -> Result<&str, FileError> {
        let budget = self
            .capacity()
            .checked_sub(suffix_units)
            .ok_or(FileError::PathTooLong)?;
        if self.units(stem) <= budget {
            return Ok(stem);
        }
        let mut remaining = budget;
        let mut end = 0;
        for cluster in stem.graphemes(true) {
            let Some(next) = remaining.checked_sub(self.units(cluster)) else {
                break;
            };
            end += cluster.len();
            remaining = next;
        }
        if matches!(self, Self::Windows { .. }) {
            if end == 0 {
                return Err(FileError::PathTooLong);
            }
            // Truncating e.g. "Console" to "Con" would turn the output into
            // a DOS device even after appending ".en.txt".
            let prefix = stem[..end].trim_end_matches(' ');
            if is_windows_device(prefix) {
                end = prefix.char_indices().next_back().unwrap().0;
            }
        }
        Ok(&stem[..end])
    }
}

fn is_windows_device(stem: &str) -> bool {
    let name = stem.to_ascii_uppercase();
    if matches!(
        name.as_str(),
        "CON" | "PRN" | "AUX" | "NUL" | "CONIN$" | "CONOUT$"
    ) {
        return true;
    }
    let port = name
        .strip_prefix("COM")
        .or_else(|| name.strip_prefix("LPT"));
    matches!(
        port,
        Some("1" | "2" | "3" | "4" | "5" | "6" | "7" | "8" | "9" | "¹" | "²" | "³")
    )
}

pub(super) fn native_units(name: &OsStr) -> usize {
    #[cfg(windows)]
    {
        use std::os::windows::ffi::OsStrExt;
        name.encode_wide().count()
    }
    #[cfg(not(windows))]
    {
        name.len()
    }
}

/// Suggested path for both the one-click export and the native save dialog.
pub fn translation_path(
    source: &Path,
    target: &str,
    layout_pdf: bool,
) -> Result<PathBuf, FileError> {
    numbered_translation_path(source, target, layout_pdf, 1)
}

pub(super) fn numbered_translation_path(
    source: &Path,
    target: &str,
    layout_pdf: bool,
    number: u64,
) -> Result<PathBuf, FileError> {
    let pdf_text = source
        .extension()
        .is_some_and(|e| e.eq_ignore_ascii_case("pdf"))
        && !crate::pdf::supports_pdf(target);
    let layout_pdf = layout_pdf && !pdf_text;
    let mut suffix = OsString::from(format!(".{target}"));
    if layout_pdf {
        suffix.push(".layout");
    }
    if number > 1 {
        suffix.push(format!(" ({number})"));
    }
    let extension = if pdf_text {
        Some(OsStr::new("txt"))
    } else if layout_pdf {
        Some(OsStr::new("pdf"))
    } else {
        source.extension()
    };
    if let Some(extension) = extension {
        suffix.push(".");
        suffix.push(extension);
    }
    let rules = NameRules::for_destination(source)?;
    let stem = source.file_stem().unwrap_or_default();
    let mut name = match stem.to_str() {
        Some(text) => OsStr::new(rules.fit_stem(text, native_units(&suffix))?),
        // Preserve opaque names verbatim. save_new also validates Windows paths
        // supplied directly by the picker, including non-Unicode names.
        None => stem,
    }
    .to_owned();
    name.push(suffix);
    Ok(source.with_file_name(name))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn windows_reserves_the_separator_nul_and_full_suffix() {
        let root = NameRules::Windows { prefix_units: 3 };
        assert_eq!(root.capacity(), 255);
        let rules = NameRules::Windows { prefix_units: 240 };
        let suffix = ".ja.layout (10).pdf";
        assert_eq!(rules.capacity(), suffix.len());
        assert!(matches!(
            rules.fit_stem("a", suffix.len()),
            Err(FileError::PathTooLong)
        ));
        assert_eq!(rules.fit_stem("Console", 16).unwrap(), "Co");
        assert_eq!(rules.fit_stem("lpt²data", 15).unwrap(), "lpt");
        for prefix_units in [259, 265] {
            assert_eq!(NameRules::Windows { prefix_units }.capacity(), 0);
        }
    }

    #[test]
    fn utf16_and_utf8_keep_the_longest_complete_grapheme_prefix() {
        let rules = NameRules::Windows { prefix_units: 239 };
        let stem = "日😀e\u{301}👩‍💻z";
        // Windows has 20 units left, Unix has 255 bytes. Reserve all but
        // the indicated budget for the suffix to exercise each exact cut.
        for (units, expected) in [
            (1, "日"),
            (2, "日"),
            (3, "日😀"),
            (4, "日😀"),
            (5, "日😀e\u{301}"),
            (9, "日😀e\u{301}"),
            (10, "日😀e\u{301}👩‍💻"),
            (11, stem),
        ] {
            assert_eq!(rules.fit_stem(stem, 20 - units).unwrap(), expected);
        }
        assert_eq!(NameRules::Unix.fit_stem(stem, 255 - 6).unwrap(), "日");
        assert_eq!(NameRules::Unix.fit_stem(stem, 255 - 7).unwrap(), "日😀");
    }
}
