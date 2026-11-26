use super::{FileNotice, Operation, Path, PathBuf};
use std::{cmp::Ordering, collections::HashSet, fs};

const FOLDER_LIMIT: usize = 200;

#[derive(Default)]
pub(super) struct SelectionNotes {
    translated: usize,
    duplicates: usize,
    excess: usize,
}

impl SelectionNotes {
    pub(super) fn lines(&self) -> Vec<String> {
        [
            (self.translated, "files.skip_translated"),
            (self.duplicates, "files.skip_duplicates"),
            (self.excess, "files.folder_limit"),
        ]
        .into_iter()
        .filter(|(count, _)| *count > 0)
        .map(|(count, key)| rust_i18n::t!(key, count = count, limit = FOLDER_LIMIT).into_owned())
        .collect()
    }
}

#[derive(Default)]
pub(super) struct Selection {
    pub inputs: Vec<(PathBuf, Option<FileNotice>)>,
    pub notes: SelectionNotes,
    pub grouped: bool,
}

fn supported(path: &Path) -> bool {
    path.extension().is_some_and(|extension| {
        ["pdf", "docx", "pptx", "md", "txt"]
            .iter()
            .any(|ext| extension.eq_ignore_ascii_case(ext))
    })
}

fn translated_name(path: &Path) -> bool {
    let Some(stem) = path.file_stem().and_then(|s| s.to_str()) else {
        return false;
    };
    let stem = stem
        .strip_suffix(')')
        .and_then(|s| s.rsplit_once(" ("))
        .filter(|(_, number)| number.parse::<u64>().is_ok_and(|n| n >= 2))
        .map_or(stem, |(stem, _)| stem);
    let extension = path.extension().and_then(|ext| ext.to_str()).unwrap_or("");
    let sibling = |base: &str| path.with_file_name(format!("{base}.{extension}")).is_file();
    let Some((base, suffix)) = stem.rsplit_once('.') else {
        return false;
    };
    let language = |suffix: &str| {
        std::iter::once(engine::pfml::JA_EASY)
            .chain(engine::pfml::LANGUAGES.iter().map(|l| l.code))
            .any(|code| suffix.eq_ignore_ascii_case(code))
            // Such as Portuguese, which only an external model translates.
            || engine::profiles::external_language_code(&suffix.to_ascii_lowercase()).is_some()
    };
    if suffix.eq_ignore_ascii_case("layout") && extension.eq_ignore_ascii_case("pdf") {
        return sibling(base)
            || base
                .rsplit_once('.')
                .is_some_and(|(name, lang)| language(lang) && sibling(name));
    }
    language(suffix)
        && (sibling(base)
            || (extension.eq_ignore_ascii_case("txt")
                && !::files::pdf::supports_pdf(suffix)
                && path.with_file_name(format!("{base}.pdf")).is_file()))
}

fn hidden(meta: &fs::Metadata) -> bool {
    #[cfg(windows)]
    {
        use std::os::windows::fs::MetadataExt;
        meta.file_attributes() & 2 != 0
    }
    #[cfg(target_os = "macos")]
    {
        use std::os::macos::fs::MetadataExt;
        meta.st_flags() & 0x8000 != 0
    }
    #[cfg(not(any(windows, target_os = "macos")))]
    {
        let _ = meta;
        false
    }
}

// Compare digit runs by significant length then digits, including runs too long for u64.
fn natural_order(mut a: &[u8], mut b: &[u8]) -> Ordering {
    while let (Some(&x), Some(&y)) = (a.first(), b.first()) {
        if x.is_ascii_digit() && y.is_ascii_digit() {
            let (left, rest_a) = a.split_at(a.iter().take_while(|c| c.is_ascii_digit()).count());
            let (right, rest_b) = b.split_at(b.iter().take_while(|c| c.is_ascii_digit()).count());
            let significant = |s: &[u8]| s.iter().take_while(|c| **c == b'0').count();
            let left = &left[significant(left)..];
            let right = &right[significant(right)..];
            let ord = left.len().cmp(&right.len()).then_with(|| left.cmp(right));
            if ord != Ordering::Equal {
                return ord;
            }
            (a, b) = (rest_a, rest_b);
        } else {
            if x != y {
                return x.cmp(&y);
            }
            a = &a[1..];
            b = &b[1..];
        }
    }
    a.len().cmp(&b.len())
}

fn order(a: &Path, b: &Path) -> Ordering {
    let name = |p: &Path| {
        p.file_name()
            .unwrap_or_default()
            .to_string_lossy()
            .to_lowercase()
    };
    natural_order(name(a).as_bytes(), name(b).as_bytes()).then_with(|| a.cmp(b))
}

#[derive(Default)]
struct Seen {
    paths: HashSet<PathBuf>,
    #[cfg(unix)]
    files: HashSet<(u64, u64)>,
    #[cfg(windows)]
    files: HashSet<same_file::Handle>,
}

impl Seen {
    fn insert(&mut self, path: &Path) -> bool {
        if !self.paths.insert(path.to_owned()) {
            return false;
        }
        #[cfg(unix)]
        let id = {
            use std::os::unix::fs::MetadataExt;
            path.metadata().map(|m| (m.dev(), m.ino()))
        };
        #[cfg(windows)]
        let id = same_file::Handle::from_path(path);
        id.map_or(true, |id| self.files.insert(id))
    }
}

pub(super) fn expand_paths(paths: Vec<PathBuf>, existing: Vec<PathBuf>) -> Selection {
    let mut result = Selection {
        grouped: paths.len() > 1,
        ..Default::default()
    };
    let mut seen = Seen::default();
    for path in existing {
        seen.insert(&path);
    }
    for path in paths {
        if !path.is_dir() {
            let error = (!supported(&path)).then(|| FileNotice::text("files.unsupported"));
            result.inputs.push((path, error));
            continue;
        }
        result.grouped = true;
        let mut entries = Vec::new();
        let mut translated = 0;
        let read = (|| -> std::io::Result<()> {
            for child in fs::read_dir(&path)? {
                let child = child?.path();
                let name = child.file_name().unwrap_or_default().to_string_lossy();
                if !supported(&child) || name.starts_with('.') || name.starts_with("~$") {
                    continue;
                }
                match child.metadata() {
                    Ok(m) if !m.is_file() || m.len() == 0 || hidden(&m) => {}
                    Ok(_) if translated_name(&child) => translated += 1,
                    Ok(_) => entries.push((child, None)),
                    Err(e) => {
                        entries.push((child, Some(FileNotice::error(&e.into(), Operation::Open))))
                    }
                }
            }
            Ok(())
        })();
        entries.sort_by(|a, b| order(&a.0, &b.0));
        result.notes.excess += entries.len().saturating_sub(FOLDER_LIMIT);
        entries.truncate(FOLDER_LIMIT);
        if let Err(e) = read {
            entries.push((path, Some(FileNotice::error(&e.into(), Operation::Open))));
        } else if entries.is_empty() && translated == 0 {
            entries.push((path, Some(FileNotice::text("files.folder_empty"))));
        }
        result.notes.translated += translated;
        result.inputs.extend(entries);
    }
    result.inputs.sort_by(|a, b| order(&a.0, &b.0));
    result.inputs.retain(|(path, _)| {
        let new = seen.insert(path);
        result.notes.duplicates += usize::from(!new);
        new
    });
    result
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn recognizes_only_output_suffixes() {
        let root = std::env::temp_dir().join(format!("plamo-suffix-{}", std::process::id()));
        fs::create_dir_all(&root).unwrap();
        for name in ["r.txt", "r.docx", "r.md", "r.pdf"] {
            fs::write(root.join(name), b"sample").unwrap();
        }
        for name in [
            "r.en.txt",
            "r.pt.md",
            "r.ZH-Hant (2).docx",
            "r.ja(easy).md",
            "r.layout.pdf",
            "r.ja.layout (10).pdf",
        ] {
            assert!(translated_name(&root.join(name)), "{name}");
        }
        for name in [
            "report.v2.pdf",
            "notes.en.md",
            "report.ja.pdf",
            "manual.layout.pdf",
            "r.layout.txt",
            "r (2).pdf",
            "r.minutes.md",
            "a.en (notes).txt",
        ] {
            assert!(!translated_name(&root.join(name)), "{name}");
        }
        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn folder_and_picker_use_natural_order_filter_and_report_omissions() {
        let root = std::env::temp_dir().join(format!("plamo-selection-{}", std::process::id()));
        fs::create_dir_all(root.join("sub")).unwrap();
        for name in [
            "Ch10.md",
            "ch2.md",
            "a.pdf",
            "a.ja.pdf",
            ".hide.pdf",
            "._a.txt",
            "~$a.docx",
            "sub/deep.md",
            "a.xlsx",
        ] {
            fs::write(root.join(name), b"sample").unwrap();
        }
        fs::write(root.join("empty.pdf"), []).unwrap();
        let result = expand_paths(
            vec![root.clone(), root.join("a.ja.pdf"), root.join("Ch10.md")],
            vec![],
        );
        assert_eq!(result.notes.translated, 1);
        assert_eq!(result.notes.duplicates, 1);
        assert_eq!(
            result.inputs.into_iter().map(|e| e.0).collect::<Vec<_>>(),
            ["a.ja.pdf", "a.pdf", "ch2.md", "Ch10.md"].map(|name| root.join(name))
        );
        fs::hard_link(root.join("ch2.md"), root.join("alias.md")).unwrap();
        assert_eq!(
            expand_paths(vec![root.join("alias.md")], vec![root.join("ch2.md")])
                .notes
                .duplicates,
            1
        );
        for i in 0..202 {
            fs::write(root.join(format!("a{i}.txt")), b"x").unwrap();
        }
        let result = expand_paths(vec![root.clone()], vec![]);
        assert_eq!(result.notes.excess, 6);
        assert_eq!(result.inputs.len(), 200);
        assert_eq!(result.inputs.last().unwrap().0, root.join("a198.txt"));
        fs::remove_dir_all(root).unwrap();
    }
}
