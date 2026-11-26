//! Local text-layer PDF translation. The original pages are copied into the output.
use std::io::Read;
use std::path::{Path, PathBuf};
use std::sync::{Mutex, MutexGuard};

use pdfium_render::prelude::*;
use thiserror::Error;

#[cfg(feature = "pdf-erase")]
pub mod erase;
#[cfg(feature = "pdf-erase")]
pub mod export;
mod fonts;
pub mod inplace;
pub mod layout;
use fonts::{Faces, Noto};
pub use fonts::{Family, supports_pdf};
mod parser;
mod subset;
mod unicode;

const MAX_BYTES: u64 = 32 * 1024 * 1024;
const MAX_PAGES: u16 = 100;
const MAX_PAGE_CHARS: usize = 32_000;
const MAX_TOTAL_CHARS: usize = 400_000;
const WIDTH: f32 = 595.0;
const HEIGHT: f32 = 842.0;
const MARGIN: f32 = 44.0;
const SIZE: f32 = 11.0;
const LEADING: f32 = 16.0;
const CONTENT: f32 = WIDTH - 2.0 * MARGIN;

#[derive(Debug, Error)]
pub enum Error {
    #[error(
        "PDFium is unavailable; use the local PDF-enabled app or configure PRIVATE_TRANSLATE_PDFIUM_LIBRARY"
    )]
    Library,
    #[error(
        "a Japanese-capable TrueType font is required; set PRIVATE_TRANSLATE_PDF_FONT or install IPA Gothic"
    )]
    Font,
    #[error("this language requires text export instead of PDF")]
    UnsupportedLanguage,
    #[error("PDF exceeds the local preview size or page limit")]
    Limit,
    #[error("a password is required to open this PDF")]
    Encrypted,
    #[error("editing is restricted for this PDF, so a translated PDF cannot be created")]
    EditingRestricted,
    #[error("protected PDF files are unsupported")]
    Protected,
    #[error("signed PDFs and interactive PDF forms are unsupported")]
    Interactive,
    #[error("PDF could not be processed: {0}")]
    Pdf(#[source] PdfiumError),
}

impl From<PdfiumError> for Error {
    fn from(error: PdfiumError) -> Self {
        if matches!(
            error,
            PdfiumError::PdfiumLibraryInternalError(PdfiumInternalError::PasswordError)
        ) {
            Self::Encrypted
        } else {
            Self::Pdf(error)
        }
    }
}

pub struct Pdf {
    bytes: Vec<u8>,
    font: Vec<u8>,
    noto: Noto,
    units: Vec<Unit>,
    pages: Vec<Page>,
}

pub struct Unit {
    pub text: String,
    pub page: usize,
}

struct Page {
    rows: Vec<Vec<Cell>>,
    skipped: bool,
    has_text: bool,
}

struct Cell {
    index: usize,
    start: f32,
    /// Left, bottom, right, top in PDF user space: developer diagnostics only.
    bounds: [f32; 4],
    /// The page (from 1) where its text ends: the next one for a paragraph that goes on at
    /// the top of the next page.
    last_page: usize,
}

#[derive(Clone, Debug)]
struct Glyph {
    ch: char,
    x: f32,
    right: f32,
    y: f32,
    size: f32,
    bold: bool,
}

struct Col {
    text: String,
    x: f32,
    right: f32,
    /// Unshifted left, bottom, right, top of the glyphs (diagnostics and page margins).
    bounds: [f32; 4],
    /// The left of its lines after the first (a first-line or a hanging indent apart).
    cont: f32,
    lines: usize,
    /// The size and baseline of most of its first line's glyphs.
    size: f32,
    baseline: f32,
    /// Whether its last line reached the right edge of its column (set by `fold_lines`).
    full: bool,
    /// Most of its first line is bold; the first line mixes bold and regular words.
    bold: bool,
    mixed: bool,
    weights: (usize, usize),
    /// The page (from 0) where its text ends, when it goes on at the top of the next page.
    ends: Option<usize>,
}

impl Col {
    fn new(text: String, x: f32, right: f32, bounds: [f32; 4]) -> Self {
        Self {
            text,
            x,
            right,
            bounds,
            cont: x,
            lines: 1,
            size: 0.0,
            baseline: 0.0,
            full: false,
            bold: false,
            mixed: false,
            weights: (0, 0),
            ends: None,
        }
    }
}

fn glyph_bounds(g: &Glyph) -> [f32; 4] {
    [g.x, g.y - 0.25 * g.size, g.right, g.y + 0.8 * g.size]
}

fn union_bounds(a: &mut [f32; 4], b: [f32; 4]) {
    a[0] = a[0].min(b[0]);
    a[1] = a[1].min(b[1]);
    a[2] = a[2].max(b[2]);
    a[3] = a[3].max(b[3]);
}

struct Line {
    cols: Vec<Col>,
    y: f32,
    size: f32,
}

fn resource_dirs(exe: &Path, macos: bool) -> Vec<PathBuf> {
    let mut dirs = Vec::new();
    if macos
        && let Some(contents) = exe.parent().and_then(Path::parent)
        && contents.file_name().is_some_and(|name| name == "Contents")
    {
        dirs.push(contents.join("Resources"));
    }
    if let Some(parent) = exe.parent() {
        dirs.push(parent.to_path_buf());
    }
    dirs
}

fn resource_candidates(
    exe: &Path,
    macos: bool,
    names: &[&str],
    explicit: Option<PathBuf>,
    home: Option<&Path>,
) -> Vec<PathBuf> {
    if let Some(path) = explicit {
        return vec![path];
    }
    let mut paths: Vec<_> = resource_dirs(exe, macos)
        .into_iter()
        .flat_map(|dir| names.iter().map(move |name| dir.join(name)))
        .collect();
    if macos && let Some(home) = home {
        paths.push(home.join("Library/Fonts/ipag.ttf"));
    }
    paths
}

fn resource_path(variable: &str, names: &[&str], font: bool) -> Option<PathBuf> {
    let explicit = std::env::var_os(variable).map(PathBuf::from);
    let is_explicit = explicit.is_some();
    let exe = std::env::current_exe().unwrap_or_default();
    let home = font
        .then(|| std::env::var_os("HOME"))
        .flatten()
        .map(PathBuf::from);
    resource_candidates(
        &exe,
        cfg!(target_os = "macos"),
        names,
        explicit,
        home.as_deref(),
    )
    .into_iter()
    .find(|path| is_explicit || path.is_file())
}

fn pdfium() -> Result<(MutexGuard<'static, ()>, Pdfium), Error> {
    // 0.8 lacks the library-wide Send/Sync guarantees. Keep initialization,
    // documents and shutdown in one call, and never overlap two instances.
    static PDFIUM: Mutex<()> = Mutex::new(());
    let guard = PDFIUM.lock().unwrap_or_else(|error| {
        PDFIUM.clear_poison();
        error.into_inner()
    });
    let bind = || {
        let name = if cfg!(target_os = "macos") {
            "libpdfium.dylib"
        } else if cfg!(windows) {
            "pdfium.dll"
        } else {
            "libpdfium.so"
        };
        let path = resource_path("PRIVATE_TRANSLATE_PDFIUM_LIBRARY", &[name], false)?;
        if !path.is_absolute() || !path.is_file() {
            return None;
        }
        Pdfium::bind_to_library(&path).ok().map(Pdfium::new)
    };
    Ok((guard, bind().ok_or(Error::Library)?))
}

fn output_font() -> Result<Vec<u8>, Error> {
    let file = resource_path(
        "PRIVATE_TRANSLATE_PDF_FONT",
        &["ipafont/ipag.ttf", "pdf-font.ttf"],
        true,
    )
    .ok_or(Error::Font)?;
    let f = std::fs::File::open(file).map_err(|_| Error::Font)?;
    let mut bytes = Vec::new();
    f.take(20 * 1024 * 1024)
        .read_to_end(&mut bytes)
        .map_err(|_| Error::Font)?;
    let font = ttf_parser::Face::parse(&bytes, 0).map_err(|_| Error::Font)?;
    if font.glyph_index('日').is_none() || font.glyph_index('A').is_none() {
        return Err(Error::Font);
    }
    Ok(bytes)
}

/// Admit unprotected, unsigned PDFs within the page limit. An Acrobat form must
/// have a verifiably empty field tree, and every page must be free of widgets.
fn admit(doc: &PdfDocument<'_>, bytes: &[u8]) -> Result<(), Error> {
    if !matches!(
        doc.permissions().security_handler_revision(),
        Ok(PdfSecurityHandlerRevision::Unprotected)
    ) {
        // The file opened without a password, so say which restriction stops the export.
        return Err(if may_modify(doc.bindings(), bytes) {
            Error::Protected
        } else {
            Error::EditingRestricted
        });
    }
    if !doc.signatures().is_empty() {
        return Err(Error::Interactive);
    }
    if doc.pages().len() > MAX_PAGES {
        return Err(Error::Limit);
    }
    match doc.form().map(|form| form.form_type()) {
        None | Some(PdfFormType::None) => {}
        Some(PdfFormType::Acrobat)
            if matches!(empty_acroform(bytes, doc.pages().len()), Ok(true)) => {}
        _ => return Err(Error::Interactive),
    }
    // A widget can exist on a page even without a catalog field tree.
    for index in doc.pages().as_range() {
        let page = doc.pages().get(index)?;
        for index in page.annotations().as_range() {
            if matches!(
                page.annotations().get(index)?.annotation_type(),
                PdfPageAnnotationType::Widget | PdfPageAnnotationType::XfaWidget
            ) {
                return Err(Error::Interactive);
            }
        }
    }
    Ok(())
}

/// PDFium calls even an empty field tree an Acrobat form. Verify its catalog and
/// page annotations before admitting it. Unresolvable entries keep the old refusal.
fn empty_acroform(bytes: &[u8], page_count: u16) -> Result<bool, parser::Error> {
    parser::with_document(bytes, |pdf| {
        let catalog = pdf.catalog()?;
        let form = catalog.get_deref(b"AcroForm", &pdf)?.as_dict()?;
        if catalog.has(b"Perms")
            || form.has(b"SigFlags")
            || form.has(b"XFA")
            || (form.has(b"Fields") && !form.get_deref(b"Fields", &pdf)?.as_array()?.is_empty())
        {
            return Ok(false);
        }
        let pages = pdf.get_pages();
        if pages.len() != usize::from(page_count) {
            return Ok(false);
        }
        for page in pages.values() {
            let page = pdf.get_dictionary(*page)?;
            if page.has(b"Annots") {
                for annot in page.get_deref(b"Annots", &pdf)?.as_array()? {
                    let dict = pdf.dereference(annot)?.1.as_dict()?;
                    if dict.get_deref(b"Subtype", &pdf)?.as_name()? == b"Widget" {
                        return Ok(false);
                    }
                }
            }
        }
        Ok(true)
    })
}

/// Whether a protected PDF still allows its contents to be changed. This reads the permission
/// bits directly: the wrapper only knows security handlers up to revision 4, and AES-256 files
/// use revisions 5 and 6. Bit 4 means the same in every revision.
fn may_modify(lib: &dyn PdfiumLibraryBindings, bytes: &[u8]) -> bool {
    const MODIFY_CONTENTS: u32 = 1 << 3;
    let raw = lib.FPDF_LoadMemDocument64(bytes, None);
    if raw.is_null() {
        return true;
    }
    let bits = lib.FPDF_GetDocPermissions(raw) as u32;
    lib.FPDF_CloseDocument(raw);
    bits & MODIFY_CONTENTS != 0
}

fn pt(x: f32) -> PdfPoints {
    PdfPoints::new(x)
}

impl Pdf {
    pub fn read(path: &Path) -> Result<Self, crate::FileError> {
        let mut bytes = Vec::new();
        let f = std::fs::File::open(path)?;
        if f.metadata()?.len() > MAX_BYTES {
            return Err(Error::Limit.into());
        }
        f.take(MAX_BYTES + 1).read_to_end(&mut bytes)?;
        if bytes.len() as u64 > MAX_BYTES {
            return Err(Error::Limit.into());
        }
        let (units, pages) = Self::parse(&bytes)?;
        let font = if units.is_empty() {
            Vec::new()
        } else {
            output_font()?
        };
        Ok(Self {
            bytes,
            font,
            noto: Noto::load(),
            units,
            pages,
        })
    }

    fn parse(bytes: &[u8]) -> Result<(Vec<Unit>, Vec<Page>), Error> {
        let (_guard, pdfium) = pdfium()?;
        let doc = pdfium.load_pdf_from_byte_slice(bytes, None)?;
        admit(&doc, bytes)?;

        let mut units = Vec::new();
        let mut pages = Vec::new();
        let mut count = 0;
        // Each page's glyphs first: the line rhythm is the document's.
        let mut read = Vec::new();
        for page in doc.pages().iter() {
            let mut glyphs = Vec::new();
            let mut skipped = !page.annotations().is_empty();
            let text = page.text()?;
            if text.chars().len() > MAX_PAGE_CHARS {
                return Err(Error::Limit);
            }
            for g in text.chars().iter() {
                let Some(mut ch) = g.unicode_char() else {
                    skipped = true;
                    continue;
                };
                // PDFium gives a hyphen at the end of a line as U+0002: a hyphen that the
                // next line may join again (`compli-` `cated`).
                if ch == '\u{2}' && !g.is_generated().unwrap_or(false) {
                    ch = '-';
                }
                if g.is_generated().unwrap_or(false) || ch.is_control() {
                    continue;
                }
                if let Some(mark) = layout::marker_char(ch) {
                    ch = mark;
                }
                if ch == '\u{fffd}'
                    || ('\u{e000}'..='\u{f8ff}').contains(&ch)
                    || g.angle_degrees().map_or(true, |a| a.abs() > 2.0)
                {
                    skipped = true;
                    continue;
                }
                let (Ok((x, y)), Ok(b)) = (g.origin(), g.loose_bounds()) else {
                    skipped = true;
                    continue;
                };
                let size = g.scaled_font_size().value.abs();
                if [x.value, y.value, b.right().value, size]
                    .into_iter()
                    .all(f32::is_finite)
                    && size > 0.0
                {
                    // As the layout export tells bold: by the weight or the font's name.
                    let bold =
                        g.font_weight().is_some_and(|w| {
                            matches!(
                                w,
                                PdfFontWeight::Weight600
                                    | PdfFontWeight::Weight700Bold
                                    | PdfFontWeight::Weight800
                                    | PdfFontWeight::Weight900
                            )
                        }) || layout::font_kind(&g.font_name(), layout::FontFlags::default()).bold;
                    glyphs.push(Glyph {
                        ch,
                        x: x.value,
                        y: y.value,
                        right: b.right().value.max(x.value),
                        size,
                        bold,
                    });
                }
            }
            count += glyphs.len();
            if count > MAX_TOTAL_CHARS {
                return Err(Error::Limit);
            }
            read.push((glyphs, skipped, page.width().value, page.height().value));
        }
        let rhythm = rhythm(read.iter().map(|(glyphs, ..)| glyphs.as_slice()));
        let mut grouped = Vec::new();
        for (glyphs, skipped, width, height) in read {
            let needs_review = uncertain_geometry(&glyphs, width);
            let (blocks, estimated) = group_page(glyphs, width, rhythm);
            grouped.push((blocks, skipped || needs_review || estimated, width, height));
        }
        join_across_pages(&mut grouped);
        for (i, (blocks, skipped, width, _)) in grouped.into_iter().enumerate() {
            let mut rows = Vec::new();
            for row in blocks {
                let start = if row.len() == 1 {
                    row[0].x
                } else {
                    width * 0.04
                };
                rows.push(
                    row.into_iter()
                        .map(|col| {
                            let index = units.len();
                            units.push(Unit {
                                text: col.text,
                                page: i + 1,
                            });
                            Cell {
                                index,
                                start: ((col.x - start) / width).clamp(0.0, 0.88),
                                bounds: col.bounds,
                                last_page: col.ends.unwrap_or(i) + 1,
                            }
                        })
                        .collect(),
                );
            }
            pages.push(Page {
                has_text: !rows.is_empty(),
                rows,
                skipped,
            });
        }
        Ok((units, pages))
    }

    pub fn units(&self) -> &[Unit] {
        &self.units
    }

    /// Each unit's glyph bounds (left, bottom, right, top in PDF user space), in the order
    /// of [`Self::units`]. For developer diagnostics of unit boundaries; never used to write.
    pub fn unit_bounds(&self) -> Vec<[f32; 4]> {
        let mut bounds = vec![[0.0; 4]; self.units.len()];
        for cell in self.pages.iter().flat_map(|p| p.rows.iter().flatten()) {
            bounds[cell.index] = cell.bounds;
        }
        bounds
    }

    /// Units whose target will actually be drawn, rather than retained for an
    /// empty result or missing glyph. Indices refer to units(), starting at zero.
    pub fn applied_units(&self, targets: &[Option<String>], language: &str) -> Vec<usize> {
        let face = Faces::new(&self.font, &self.noto, Family::for_language(language)).ok();
        self.units
            .iter()
            .zip(targets)
            .enumerate()
            .filter_map(|(i, (unit, target))| {
                (drawable(target.as_deref(), face.as_ref())
                    && target
                        .as_ref()
                        .is_some_and(|t| t.trim() != unit.text.trim()))
                .then_some(i)
            })
            .collect()
    }

    /// Preserve a source cell if a recognized yen amount changes in translation.
    pub fn check_amounts(&self, targets: &mut [Option<String>], language: &str) {
        for (unit, target) in self.units.iter().zip(targets) {
            if target
                .as_ref()
                .is_some_and(|t| !crate::office::amounts::pdf_matches(&unit.text, t, language))
            {
                *target = None;
            }
        }
    }

    pub fn review_pages(&self, targets: &[Option<String>], language: &str) -> Vec<usize> {
        let mut warnings: Vec<_> = self
            .pages
            .iter()
            .enumerate()
            .filter_map(|(i, page)| (!page.has_text || page.skipped).then_some(i + 1))
            .collect();
        let face = Faces::new(&self.font, &self.noto, Family::for_language(language)).ok();
        // A unit that goes on at the top of the next page: both its pages.
        warnings.extend(
            self.pages
                .iter()
                .flat_map(|page| page.rows.iter().flatten())
                .filter(|cell| !usable(targets[cell.index].as_deref(), face.as_ref()))
                .flat_map(|cell| [self.units[cell.index].page, cell.last_page]),
        );
        warnings.sort_unstable();
        warnings.dedup();
        warnings
    }

    pub fn write(
        &self,
        targets: &[Option<String>],
        target: &str,
    ) -> Result<Vec<u8>, crate::FileError> {
        if targets.len() != self.units.len() || targets.is_empty() {
            return Err(crate::FileError::MismatchedResults);
        }
        if !supports_pdf(target) {
            return Err(Error::UnsupportedLanguage.into());
        }
        self.write_pdf(targets, target).map_err(Into::into)
    }

    fn write_pdf(&self, targets: &[Option<String>], target: &str) -> Result<Vec<u8>, Error> {
        let (_guard, pdfium) = pdfium()?;
        let source = pdfium.load_pdf_from_byte_slice(&self.bytes, None)?;
        let mut output = pdfium.create_new_pdf()?;
        let face = Faces::new(&self.font, &self.noto, Family::for_language(target))?;
        let mut sheets = Vec::new();
        let japanese = target == "ja";
        // Pages where a unit begun on the page before ends, not translated.
        let ended: Vec<usize> = self
            .pages
            .iter()
            .flat_map(|page| page.rows.iter().flatten())
            .filter(|c| !usable(targets[c.index].as_deref(), Some(&face)))
            .map(|c| c.last_page)
            .collect();
        for (i, page) in self.pages.iter().enumerate() {
            if !page.has_text {
                sheets.push(Vec::new());
                continue;
            }
            let review = page.skipped
                || ended.contains(&(i + 1))
                || page
                    .rows
                    .iter()
                    .flatten()
                    .any(|c| !usable(targets[c.index].as_deref(), Some(&face)));
            // The last page that its units' texts reach: the next one for a paragraph that
            // goes on at its top.
            let last = page
                .rows
                .iter()
                .flatten()
                .map(|c| c.last_page - 1)
                .max()
                .unwrap_or(i)
                .max(i);
            // Render translated cells in their source row; wrapped cells keep their own column.
            let mut lines = Vec::new();
            for row in &page.rows {
                let stacked = row
                    .windows(2)
                    .any(|cells| (cells[1].start - cells[0].start) * CONTENT < 34.0);
                let mut drawn: Vec<Vec<(f32, String)>> = Vec::new();
                for (j, cell) in row.iter().enumerate() {
                    let left = if stacked { 0.0 } else { cell.start * CONTENT };
                    let right = row
                        .get(j + 1)
                        .map_or(CONTENT, |next| (next.start * CONTENT).max(left + 1.0));
                    let unit = &self.units[cell.index];
                    let candidate = targets[cell.index].as_deref();
                    let retained = if japanese {
                        "[原本参照]"
                    } else {
                        "[See original]"
                    };
                    // Keep JA/EN reference markers and pagination for a retained
                    // source cell. The target always gets character-level fallback.
                    let source_usable = if face.ipa_first {
                        unit.text
                            .chars()
                            .all(|ch| ch.is_whitespace() || face.fonts[0].glyph_index(ch).is_some())
                    } else {
                        usable(Some(&unit.text), Some(&face))
                    };
                    let text = if drawable(candidate, Some(&face)) {
                        candidate.unwrap_or(retained)
                    } else if source_usable {
                        &unit.text
                    } else {
                        retained
                    };
                    let text = text.split_whitespace().collect::<Vec<_>>().join(" ");
                    let chunks = wrap(
                        &text,
                        &face,
                        if stacked { CONTENT } else { right - left - 8.0 },
                    );
                    let offset = if stacked { drawn.len() } else { 0 };
                    for (i, chunk) in chunks.into_iter().enumerate() {
                        let k = i + offset;
                        if drawn.len() <= k {
                            drawn.push(Vec::new());
                        }
                        drawn[k].push((MARGIN + left, chunk.to_owned()));
                    }
                }
                lines.extend(drawn);
                if row.len() == 1 {
                    lines.push(Vec::new());
                }
            }
            sheets.push(translation_sheets(
                &lines,
                (i, last),
                japanese,
                review,
                false,
            ));
        }
        // Preserve PDFium's original character mapping and text operators for IPA-only pages.
        let legacy = face.ipa_first
            && sheets
                .iter()
                .flatten()
                .flatten()
                .all(|(_, _, _, s)| s.chars().all(|c| face.fonts[0].glyph_index(c).is_some()));
        let font = if legacy {
            let used: std::collections::BTreeSet<_> = sheets
                .iter()
                .flatten()
                .flatten()
                .flat_map(|(_, _, _, s)| s.chars())
                .collect();
            let (_, bytes) = subset::subset(
                &face.fonts[0],
                used.into_iter()
                    .filter_map(|ch| face.fonts[0].glyph_index(ch)),
            )?;
            Some(output.fonts_mut().load_true_type_from_bytes(&bytes, true)?)
        } else {
            None
        };
        let mut text_lines = Vec::new();
        for (i, sheets) in sheets.into_iter().enumerate() {
            let index = output.pages().len();
            output
                .pages_mut()
                .copy_page_from_document(&source, i as u16, index)?;
            for sheet in sheets {
                let page_number = usize::from(output.pages().len());
                let mut page = output
                    .pages_mut()
                    .create_page_at_end(PdfPagePaperSize::from_points(pt(WIDTH), pt(HEIGHT)))?;
                for (x, y, size, text) in sheet {
                    if let Some(font) = font {
                        page.objects_mut().create_text_object(
                            pt(x),
                            pt(y),
                            &text,
                            font,
                            pt(size),
                        )?;
                    } else {
                        text_lines.push(unicode::TextLine::at(page_number, x, y, size, text));
                    }
                }
            }
        }
        let bytes = output.save_to_bytes()?;
        if text_lines.is_empty() {
            Ok(bytes)
        } else {
            unicode::append_faces(&bytes, &face, &text_lines, true)
        }
    }
}

/// Shared A4, 11 pt translation-page typography. The default exporter uses
/// precisely the same order and positions as before layout export was added.
type Sheet = Vec<(f32, f32, f32, String)>;
/// `pages`: the original pages (from 0) that the lines translate, the first and the last
/// (the next one when a paragraph goes on at its top).
fn translation_sheets(
    lines: &[Vec<(f32, String)>],
    (original, last): (usize, usize),
    japanese: bool,
    review: bool,
    supplemental: bool,
) -> Vec<Sheet> {
    let start_y = HEIGHT - 78.0;
    let per_page = ((start_y - MARGIN) / LEADING) as usize + 1;
    let pages = if last > original {
        format!(
            "{}{}{}",
            original + 1,
            if japanese { "～" } else { "-" },
            last + 1
        )
    } else {
        (original + 1).to_string()
    };
    let page = if last > original { "pages" } else { "page" };
    lines
        .chunks(per_page)
        .enumerate()
        .map(|(sheet, chunk)| {
            let title = match (japanese, supplemental) {
                (true, false) => format!("機械翻訳  |  原本 {pages} ページ  |  {}", sheet + 1),
                (false, false) => format!(
                    "Machine translation  |  Original {page} {pages}  |  {}",
                    sheet + 1
                ),
                (true, true) => format!("補足訳文  |  原本 {pages} ページ  |  {}", sheet + 1),
                (false, true) => format!(
                    "Supplementary translation  |  Original {page} {pages}  |  {}",
                    sheet + 1
                ),
            };
            let mut out = vec![(MARGIN, HEIGHT - MARGIN, 12.0, title)];
            if review {
                out.push((
                    MARGIN,
                    HEIGHT - 61.0,
                    9.0,
                    if japanese {
                        "未翻訳の箇所や読み順は原本で確認してください。"
                    } else {
                        "Check the original for retained text and reading order."
                    }
                    .to_owned(),
                ));
            }
            for (n, line) in chunk.iter().skip_while(|line| line.is_empty()).enumerate() {
                for (x, text) in line {
                    out.push((*x, start_y - n as f32 * LEADING, SIZE, text.clone()));
                }
            }
            out
        })
        .collect()
}

fn drawable(text: Option<&str>, face: Option<&Faces<'_>>) -> bool {
    usable(text, face)
        || (face.is_some_and(|f| f.fonts.len() > 1) && text.is_some_and(|s| !s.trim().is_empty()))
}

fn usable(text: Option<&str>, face: Option<&Faces<'_>>) -> bool {
    text.is_some_and(|s| {
        !s.trim().is_empty()
            && face.is_some_and(|face| {
                s.chars()
                    .all(|c| c.is_whitespace() || face.exact(c).is_some())
            })
    })
}

fn wrap<'a>(mut text: &'a str, face: &Faces<'_>, limit: f32) -> Vec<&'a str> {
    let mut out = Vec::new();
    while !text.is_empty() {
        let mut width = 0.0;
        let mut end = text.len();
        let mut last_space = 0;
        for (i, ch) in text.char_indices() {
            width += face
                .exact(ch)
                .and_then(|(i, g)| {
                    let font = &face.fonts[i];
                    font.glyph_hor_advance(g)
                        .map(|w| f32::from(w) * (SIZE / f32::from(font.units_per_em())))
                })
                .unwrap_or(SIZE);
            if width > limit && i > 0 {
                end = if last_space > 0 { last_space } else { i };
                break;
            }
            if ch == ' ' {
                last_space = i;
            }
        }
        out.push(text[..end].trim_end());
        text = text[end..].trim_start();
    }
    out
}

fn latin_word(ch: char) -> bool {
    ch.is_ascii_alphanumeric() || matches!(ch, ')' | ']' | '%' | ',' | '.')
}

fn columns(x: &mut [Glyph]) -> Vec<Col> {
    x.sort_by(|a, b| a.x.total_cmp(&b.x));
    let mut cols: Vec<Col> = Vec::new();
    // Each column's glyph sizes and baselines, for its own main size.
    let mut sizes: Vec<Vec<(f32, f32)>> = Vec::new();
    for g in x {
        let col = cols.last_mut();
        if let Some(c) = col
            && (g.x - c.right < (g.size * 1.3).max(12.0) || g.ch.is_whitespace())
        {
            // Deduplicate a glyph accidentally drawn twice (common with faux-bold text).
            if c.text.ends_with(g.ch) && g.right > c.right - 0.5 && g.right < c.right + 0.5 {
                continue;
            }
            if !g.ch.is_whitespace()
                && g.x - c.right > g.size * 0.20
                && c.text.chars().next_back().is_some_and(latin_word)
                && latin_word(g.ch)
            {
                c.text.push(' ');
            }
            c.text.push(g.ch);
            c.right = c.right.max(g.right);
            if !g.ch.is_whitespace() {
                union_bounds(&mut c.bounds, glyph_bounds(g));
                sizes.last_mut().unwrap().push((g.size, g.y));
                if g.ch.is_alphanumeric() {
                    *if g.bold {
                        &mut c.weights.0
                    } else {
                        &mut c.weights.1
                    } += 1;
                }
            }
        } else if !g.ch.is_whitespace() {
            let mut col = Col::new(g.ch.to_string(), g.x, g.right, glyph_bounds(g));
            if g.ch.is_alphanumeric() {
                col.weights = (usize::from(g.bold), usize::from(!g.bold));
            }
            cols.push(col);
            sizes.push(vec![(g.size, g.y)]);
        }
    }
    for (c, sizes) in cols.iter_mut().zip(sizes) {
        c.text.truncate(c.text.trim_end().len());
        let (bold, regular) = c.weights;
        c.bold = bold > regular;
        c.mixed = bold >= 2 && regular >= 2;
        if let Some((size, y)) = main_size(sizes.into_iter()) {
            c.size = size;
            c.baseline = y;
        }
    }
    // A list mark drawn apart from its text goes with it: the item's text, its later lines
    // under the text, not the mark (`▪   la description …` / `si possible …`).
    let mut merged: Vec<Col> = Vec::new();
    for col in cols {
        if let Some(mark) = merged.last_mut()
            && layout::marks_only(&mark.text)
            && mark.text.chars().count() <= 3
            && col.x - mark.right < 4.0 * col.size.max(mark.size)
        {
            mark.text = format!("{} {}", mark.text, col.text);
            mark.cont = col.x;
            mark.right = col.right;
            union_bounds(&mut mark.bounds, col.bounds);
            (mark.size, mark.baseline) = (col.size, col.baseline);
            (mark.bold, mark.mixed, mark.weights) = (col.bold, col.mixed, col.weights);
            continue;
        }
        merged.push(col);
    }
    merged
}

/// The size of most glyphs (given as size and baseline) and their baseline: a raised
/// footnote number or a subscript does not give a line its size.
fn main_size(glyphs: impl Iterator<Item = (f32, f32)>) -> Option<(f32, f32)> {
    let mut sizes: Vec<(i32, usize, f32, f32)> = Vec::new();
    for (size, y) in glyphs {
        let key = (size * 10.0).round() as i32;
        match sizes.iter_mut().find(|(k, ..)| *k == key) {
            Some((_, n, ..)) => *n += 1,
            None => sizes.push((key, 1, size, y)),
        }
    }
    let mut best: Option<&(i32, usize, f32, f32)> = None;
    for s in &sizes {
        if best.is_none_or(|b| s.1 > b.1) {
            best = Some(s);
        }
    }
    best.map(|&(_, _, size, y)| (size, y))
}

fn uncertain_geometry(glyphs: &[Glyph], width: f32) -> bool {
    // Very small text, note markers and borderline gutter detection merit a page review.
    glyphs.iter().filter(|g| g.size < 6.5).count() > 5
        || glyphs.iter().any(|g| g.x < -12.0 || g.x > width + 12.0)
}

fn read_lines(mut glyphs: Vec<Glyph>) -> Vec<Line> {
    glyphs.retain(|g| !g.ch.is_control() && !matches!(g.ch, '\u{200b}' | '\u{200e}' | '\u{200f}'));
    glyphs.sort_by(|a, b| b.y.total_cmp(&a.y));
    let mut lines: Vec<Line> = Vec::new();
    let mut from = 0;
    while from < glyphs.len() {
        let y = glyphs[from].y;
        let size = glyphs[from].size;
        let end =
            from + glyphs[from..].partition_point(|g| y - g.y <= (size * 0.26).clamp(1.4, 4.5));
        let (size, y) = main_size(
            glyphs[from..end]
                .iter()
                .filter(|g| !g.ch.is_whitespace())
                .map(|g| (g.size, g.y)),
        )
        .unwrap_or((size, y));
        let cols = columns(&mut glyphs[from..end]);
        if !cols.is_empty() {
            lines.push(Line { cols, y, size });
        }
        from = end;
    }
    lines
}

/// Choose column-major order only for a sustained, clear central gutter. A
/// table's short labels/values should remain associated by row.
fn group_page(glyphs: Vec<Glyph>, width: f32, rhythm: f32) -> (Vec<Vec<Col>>, bool) {
    let mut lines = read_lines(glyphs);
    let mut omitted = false;
    // Keep a page-number footer out of rewritten prose: the translation header
    // identifies the same source page and the original footer is copied intact.
    lines.retain(|line| {
        if line.y >= 70.0 || line.cols.len() != 1 {
            return true;
        }
        let label = line.cols[0].text.trim();
        !label
            .strip_prefix("Page ")
            .or_else(|| label.strip_prefix("ページ"))
            .is_some_and(|n| !n.trim().is_empty() && n.trim().bytes().all(|b| b.is_ascii_digit()))
    });
    // Raised footnote numbers can form a separate baseline, splitting the preceding
    // sentence mid-amount or mid-word (`公開でき` `8 9` `ない`). The source page retains
    // these markers. Each piece of such a line is a number or a reference mark.
    let remove: Vec<bool> = lines
        .iter()
        .enumerate()
        .map(|(i, line)| {
            let mark = |col: &Col| {
                col.text.chars().count() <= 3
                    && col
                        .text
                        .chars()
                        .all(|c| c.is_ascii_digit() || "*∗†‡※".contains(c))
            };
            if line.cols.is_empty() || !line.cols.iter().all(mark) {
                return false;
            }
            lines.get(i + 1).is_some_and(|next| {
                next.size > line.size * 1.24
                    && line.y - next.y < next.size * 0.75
                    && line.cols.iter().all(|col| {
                        next.cols
                            .iter()
                            .any(|n| n.x <= col.x && n.right + next.size >= col.right)
                    })
            })
        })
        .collect();
    let mut ii = 0;
    lines.retain(|_| {
        let keep = !remove[ii];
        omitted |= !keep;
        ii += 1;
        keep
    });
    let candidates = lines
        .iter()
        .enumerate()
        .filter_map(|(i, line)| {
            let [left, right] = line.cols.as_slice() else {
                return None;
            };
            if left.text.chars().count() < 13
                || right.text.chars().count() < 13
                || !(width * 0.40..width * 0.67).contains(&right.x)
            {
                return None;
            }
            Some((i, right.x))
        })
        .collect::<Vec<_>>();
    if candidates.len() >= 4 {
        let (head, x) = candidates[0];
        let tail = candidates.last().map_or(head, |(i, _)| *i);
        if candidates
            .iter()
            .all(|&(_, start)| (x - start).abs() < width * 0.055)
        {
            let gutter = candidates
                .iter()
                .map(|&(_, x)| x)
                .min_by(f32::total_cmp)
                .unwrap()
                - 5.0;
            let fits = |line: &Line| {
                line.cols
                    .iter()
                    .all(|col| col.right < gutter || col.x >= gutter)
                    && line.cols.len() <= 2
            };
            let mut first = head;
            while first > 0
                && (lines[first - 1].cols.len() == 2 || lines[first - 1].size <= lines[head].size)
                && (lines[first - 1].y - lines[first].y < lines[head].size * 4.0)
                && fits(&lines[first - 1])
            {
                first -= 1;
            }
            let mut last = tail + 1;
            while last < lines.len() && lines[last].y > 50.0 && fits(&lines[last]) {
                last += 1;
            }
            if lines[first..last].iter().all(fits) {
                let mut top = Vec::new();
                let mut left = Vec::new();
                let mut right = Vec::new();
                let mut bottom = Vec::new();
                for (i, line) in lines.into_iter().enumerate() {
                    if i < first {
                        top.push(line);
                    } else if i >= last {
                        bottom.push(line);
                    } else {
                        for mut col in line.cols {
                            let target = if col.x < gutter {
                                &mut left
                            } else {
                                col.x -= gutter;
                                col.cont -= gutter;
                                col.right -= gutter;
                                &mut right
                            };
                            // Each column's own size and baseline: a footnote beside the
                            // text of the other column does not set this column's.
                            target.push(Line {
                                y: col.baseline,
                                size: col.size,
                                cols: vec![col],
                            });
                        }
                    }
                }
                let usable_right = right
                    .iter()
                    .flat_map(|line| &line.cols)
                    .map(|col| col.right)
                    .max_by(f32::total_cmp)
                    .unwrap_or(width - gutter);
                let groups = fold_lines(top, width, rhythm)
                    .into_iter()
                    .chain(fold_lines(left, gutter, rhythm))
                    .chain(fold_lines(right, usable_right, rhythm))
                    .chain(fold_lines(bottom, width, rhythm))
                    .collect();
                return (groups, true);
            }
        }
    }
    let estimated = omitted || lines.iter().any(|line| line.cols.len() > 1);
    (fold_lines(lines, width, rhythm), estimated)
}

// Two numbers at the end of a line (a table's row), or leaders and a page number.
static ROW_END: std::sync::LazyLock<regex::Regex> = std::sync::LazyLock::new(|| {
    regex::Regex::new(r"(?:[0-9][0-9.,]*%?\s+[-+−]?[0-9][0-9.,]*%?|(?:\.{3,}|…+|・{3,})\s*[0-9]+)$")
        .expect("a valid pattern")
});

fn split_number(left: &str, right: &str) -> bool {
    (left.ends_with([',', '第']) && right.starts_with(|c: char| c.is_ascii_digit()))
        || (left.ends_with(|c: char| c.is_ascii_digit()) && right.starts_with(['万', '億', '兆']))
}

fn join_text(old: &mut Col, next: &Col) {
    join_words(&mut old.text, &next.text);
    old.right = next.right;
    union_bounds(&mut old.bounds, next.bounds);
}

fn join_words(old: &mut String, next: &str) {
    // A word hyphenated at the end of a line is joined again, as in the layout export.
    if layout::joint(old, next) == layout::Joint::Dehyphenate {
        old.truncate(old.trim_end().len() - 1);
        old.push_str(next.trim_start());
        return;
    }
    let is_cjk = |c: char| matches!(c, 'ぁ'..='ヿ' | '㐀'..='䶿' | '一'..='鿿');
    let split_number = split_number(old, next);
    let cjk = old.chars().any(is_cjk) && next.chars().any(is_cjk);
    if !cjk && !split_number {
        old.push(' ');
    }
    old.push_str(next);
}

/// The document's line rhythm, as the layout export infers it (in em, from 1.75 to 2.6):
/// lines of a paragraph further apart than 1.75 em are joined only within it.
fn rhythm<'a>(pages: impl Iterator<Item = &'a [Glyph]>) -> f32 {
    let lines: Vec<Vec<layout::Line>> = pages
        .map(|glyphs| {
            read_lines(glyphs.to_vec())
                .into_iter()
                .filter_map(|line| {
                    let [col] = line.cols.as_slice() else {
                        return None;
                    };
                    // Top-down coordinates, as the layout export's.
                    let y = -f64::from(line.y);
                    let size = f64::from(line.size);
                    let (left, right) = (f64::from(col.x), f64::from(col.right));
                    let item = layout::Item {
                        str: col.text.clone(),
                        x: left,
                        y,
                        w: right - left,
                        size,
                        top: y - 0.8 * size,
                        bottom: y + 0.2 * size,
                        font: String::new(),
                        bold: false,
                        italic: false,
                        mono: false,
                        serif: false,
                        math: false,
                        id: 0,
                    };
                    Some(layout::Line {
                        items: vec![item],
                        left,
                        right,
                        y,
                        size,
                        top: y - 0.8 * size,
                        bottom: y + 0.2 * size,
                        chars: col.text.chars().filter(|c| !c.is_whitespace()).count(),
                        math: false,
                        bold: false,
                        italic: false,
                        serif: false,
                        mono: false,
                    })
                })
                .collect()
        })
        .collect();
    layout::leading_limit(&lines).min(2.6) as f32
}

/// A paragraph that goes on at the top of the next page, in the same column and size, is one
/// unit: its last line full and unfinished, the next page's first text a lowercase word or a
/// CJK continuation. Its translation stays with the page where it begins. Running headers,
/// footers and page numbers (short text in the outer 8% of a page) are passed over.
fn join_across_pages(pages: &mut [(Vec<Vec<Col>>, bool, f32, f32)]) {
    let margin = |row: &Vec<Col>, height: f32| {
        row.iter().all(|c| {
            (c.bounds[1] > 0.92 * height || c.bounds[3] < 0.08 * height)
                && c.text.chars().count() < 60
        })
    };
    for i in 1..pages.len() {
        let (before, after) = pages.split_at_mut(i);
        let (rows, _, _, height) = &mut before[i - 1];
        let (next_rows, _, _, next_height) = &mut after[0];
        let Some(p) = rows.iter().rposition(|row| !margin(row, *height)) else {
            continue;
        };
        let Some(q) = next_rows.iter().position(|row| !margin(row, *next_height)) else {
            continue;
        };
        let ([prior], [next]) = (rows[p].as_slice(), next_rows[q].as_slice()) else {
            continue;
        };
        let size = prior.size;
        if !(prior.full
            && (prior.size - next.size).abs() < 1.0
            && ((prior.cont - next.x).abs() < 2.0 * size
                || (prior.x - next.x).abs() < 2.0 * size
                || (prior.bounds[0] - next.bounds[0]).abs() < 2.0 * size)
            && prior.text.chars().filter(|c| c.is_alphabetic()).count() >= 4
            && layout::ends_open(&prior.text)
            && layout::starts_continuation(&next.text))
        {
            continue;
        }
        let next = next_rows.remove(q).remove(0);
        join_words(&mut rows[p][0].text, &next.text);
        rows[p][0].ends = Some(i);
    }
}

// A headline can wrap before its final kana even far from the page edge.
// Do not join arbitrary short labels (e.g. 名前 / 住所) or a new sentence.
fn short_kana_tail(left: &str, right: &str) -> bool {
    left.chars().count() >= 5
        && left.chars().any(|c| matches!(c, 'ぁ'..='ヿ' | '一'..='鿿'))
        && (1..=2).contains(&right.chars().count())
        && right
            .chars()
            .all(|c| matches!(c, 'ぁ'..='ヿ' | '！' | '？'))
}

/// The right edge of the column whose lines start at `x`: the second-largest right of the
/// one-column lines starting there (one stray line apart), when there are three or more and
/// one ends short of it: lines that wrap, not labels of one width.
fn column_edge(rights: &[(f32, f32, f32)], x: f32, size: f32) -> Option<f32> {
    let mut found: Vec<f32> = rights
        .iter()
        .filter(|(left, s, _)| (left - x).abs() <= 2.0 * size && (s - size).abs() < 1.0)
        .map(|(_, _, right)| *right)
        .collect();
    if found.len() < 3 {
        return None;
    }
    found.sort_by(|a, b| b.total_cmp(a));
    let edge = found[1];
    found.iter().any(|r| *r < edge - 2.0 * size).then_some(edge)
}

fn fold_lines(lines: Vec<Line>, width: f32, rhythm: f32) -> Vec<Vec<Col>> {
    let rights: Vec<(f32, f32, f32)> = lines
        .iter()
        .filter_map(|line| match line.cols.as_slice() {
            [col] => Some((col.x, line.size, col.right)),
            _ => None,
        })
        .collect();
    // The lines that begin with a date, a time or an ordinal: a schedule's entries.
    let keys: Vec<(f64, f64, layout::Key)> = lines
        .iter()
        .filter_map(|line| match line.cols.as_slice() {
            [col] => layout::key_of(&col.text).map(|k| (f64::from(col.x), f64::from(line.size), k)),
            _ => None,
        })
        .collect();
    // Whether a line ending at `right` reached the edge of its column (lines at `xs`).
    let reaches = |right: f32, xs: [f32; 3], size: f32| {
        right > width * 0.71
            || xs
                .iter()
                .filter_map(|&x| column_edge(&rights, x, size))
                .any(|edge| right >= edge - 1.5 * size)
    };
    let mut result: Vec<Vec<Col>> = Vec::new();
    let mut past_y = 0.0;
    let mut past_size = 0.0;
    for Line { cols, y, size } in lines {
        let near = past_y - y < size * rhythm.max(1.65) && (past_size - size).abs() < 1.0;
        let mut joined = false;
        if let [next] = cols.as_slice()
            && past_y - y < size * rhythm.max(2.2)
            && let Some(previous) = result.last_mut()
        {
            let count = previous.len();
            if let Some(prior) = previous.iter_mut().find(|c| {
                let reached = reaches(c.right, [c.x, c.cont, next.x], size);
                // The first line of a paragraph indented (or a mark's line, the rest from
                // the left), or a hanging indent below a mark or a full, unfinished line.
                let indent = c.lines == 1 && c.x > next.x && c.x - next.x <= 2.0 * size;
                let hanging = c.lines == 1
                    && next.x > c.x
                    && next.x - c.x <= 2.5 * size
                    && (layout::starts_item(&c.text) || (reached && layout::ends_open(&c.text)));
                (c.cont - next.x).abs() < size
                    || (count == 1 && (indent || hanging))
                    || (near
                        && count == 1
                        && short_kana_tail(&c.text, &next.text)
                        && next.x >= c.x
                        && next.right <= c.right)
            }) && prior.text.len() + next.text.len() < 2400
            {
                let no_stop = !prior.text.ends_with(['。', '！', '？', '.', '?', '!', ':']);
                let reached = reaches(prior.right, [prior.x, prior.cont, next.x], size);
                // A bold heading does not take in the regular text below it, unless its line
                // mixes weights (bold words beginning a sentence). Bold words or a whole bold
                // line inside a sentence go on with it.
                let weight =
                    !prior.bold || next.bold || (prior.mixed && (prior.lines > 1 || reached));
                // A table's row ends with numbers, and a contents line with leaders and a page
                // number; the next row starts with a label or a number. Not the rest of a
                // sentence.
                let row = ROW_END.is_match(&prior.text)
                    && next
                        .text
                        .starts_with(|c: char| c.is_ascii_uppercase() || c.is_ascii_digit());
                // Entries of a schedule or a chronology, each line beginning with a date.
                let entry = layout::entries(
                    &keys,
                    (&prior.text, f64::from(prior.x)),
                    (&next.text, f64::from(next.x)),
                    f64::from(size),
                );
                let continuation = if count == 1 {
                    !entry
                        && (split_number(&prior.text, &next.text)
                            || (near
                                && no_stop
                                && weight
                                && !row
                                && !layout::starts_item(&next.text)
                                && (reached
                                    || (next.text.chars().count() <= 8
                                        && prior.text.chars().count() > 30)
                                    || short_kana_tail(&prior.text, &next.text))))
                } else {
                    near && no_stop && next.text.chars().count() <= 40 && past_y - y < size * 1.35
                };
                if continuation {
                    join_text(prior, next);
                    // A bold heading stays one only while its lines are all bold: a line with
                    // regular words ends its bold lead-in. The regular lines below continue the paragraph.
                    prior.bold = prior.bold && next.bold && !next.mixed;
                    if prior.lines == 1 {
                        prior.cont = next.x;
                    }
                    prior.lines += 1;
                    joined = true;
                }
            }
        }
        if !joined {
            result.push(cols);
        }
        past_y = y;
        past_size = size;
    }
    for col in result.iter_mut().flatten() {
        col.full = reaches(col.right, [col.x, col.cont, col.cont], col.size);
    }
    result
}

#[cfg(test)]
mod tests {
    #[test]
    fn joins_wrapped_kana_headline_without_joining_independent_labels() {
        use super::*;
        let line = |text: &str, y| Line {
            cols: vec![Col {
                size: 20.0,
                ..Col::new(text.into(), 100.0, 350.0, [100.0, y - 5.0, 350.0, y + 16.0])
            }],
            y,
            size: 20.0,
        };
        let out = fold_lines(
            vec![
                line("はじめてみよ", 500.0),
                line("う！", 475.0),
                line("名前", 400.0),
                line("住所", 375.0),
            ],
            780.0,
            1.75,
        );
        assert_eq!(out.len(), 3);
        assert_eq!(out[0][0].text, "はじめてみよう！");
        assert!(short_kana_tail("真夜中の紙模型列車ラウンドテーブ", "ル"));
        let mut tail = line("ル", 76.0);
        tail.cols[0].x = 160.0;
        tail.cols[0].right = 180.0;
        let centered = fold_lines(
            vec![line("真夜中の紙模型列車ラウンドテーブ", 100.0), tail],
            900.0,
            1.75,
        );
        assert_eq!(centered.len(), 1);
        assert_eq!(centered[0][0].text, "真夜中の紙模型列車ラウンドテーブル");
    }

    use super::*;

    fn line(text: &str, x: f32, y: f32) -> Vec<Glyph> {
        text.chars()
            .enumerate()
            .map(|(i, ch)| {
                let x = x + i as f32 * 6.0;
                Glyph {
                    ch,
                    x,
                    right: x + 5.0,
                    y,
                    size: 10.0,
                    bold: false,
                }
            })
            .collect()
    }

    fn form_pdf(kind: &str) -> Vec<u8> {
        use lopdf::{Document, Object, dictionary};
        let mut pdf = Document::with_version("1.7");
        let pages = pdf.new_object_id();
        let mut form = dictionary! {};
        let mut page = dictionary! {
            "Type" => "Page", "Parent" => pages, "MediaBox" => vec![0.into(), 0.into(), 612.into(), 792.into()],
        };
        let mut fields = Vec::new();
        match kind {
            "text" | "signature" | "widget" | "orphan" => {
                let field = pdf.add_object(dictionary! {
                    "FT" => if kind == "signature" { "Sig" } else { "Tx" },
                    "Subtype" => "Widget", "T" => Object::string_literal("field"),
                    "Rect" => vec![10.into(), 10.into(), 20.into(), 20.into()],
                });
                if matches!(kind, "widget" | "orphan") {
                    page.set("Annots", vec![field.into()]);
                } else {
                    fields.push(field.into());
                }
            }
            "xfa" => form.set(
                "XFA",
                pdf.add_object(lopdf::Stream::new(dictionary! {}, b"<xfa/>".to_vec())),
            ),
            "flags" => form.set("SigFlags", 1),
            "empty" | "bare" | "broken" => {}
            _ => unreachable!(),
        }
        if kind == "broken" {
            form.set("Fields", Object::Reference((999, 0)));
        } else if kind != "bare" {
            form.set("Fields", pdf.add_object(Object::Array(fields)));
        }
        let page = pdf.add_object(page);
        pdf.objects.insert(
            pages,
            dictionary! { "Type" => "Pages", "Kids" => vec![page.into()], "Count" => 1 }.into(),
        );
        let mut catalog = dictionary! { "Type" => "Catalog", "Pages" => pages };
        if kind != "orphan" {
            catalog.set("AcroForm", pdf.add_object(form));
        }
        let root = pdf.add_object(catalog);
        pdf.trailer.set("Root", root);
        let mut bytes = Vec::new();
        pdf.save_to(&mut bytes).unwrap();
        bytes
    }

    #[test]
    fn only_provably_empty_acroforms_are_admitted() {
        for kind in [
            "empty",
            "bare",
            "text",
            "signature",
            "xfa",
            "flags",
            "widget",
            "broken",
        ] {
            assert_eq!(
                matches!(empty_acroform(&form_pdf(kind), 1), Ok(true)),
                matches!(kind, "empty" | "bare"),
                "{kind}"
            );
        }
    }

    #[test]
    #[ignore = "requires the local PDFium library"]
    fn pdfium_form_admission_checks_catalog_and_page_widgets() {
        for kind in [
            "empty",
            "bare",
            "text",
            "signature",
            "xfa",
            "flags",
            "widget",
            "orphan",
            "broken",
        ] {
            let result = Pdf::parse(&form_pdf(kind));
            if matches!(kind, "empty" | "bare") {
                assert!(result.is_ok(), "{kind}");
            } else {
                assert!(matches!(result, Err(Error::Interactive)), "{kind}");
            }
        }
    }

    #[test]
    fn a_translation_page_names_both_pages_of_a_paragraph_going_on_at_the_next() {
        let lines = vec![vec![(MARGIN, "訳文".to_owned())]];
        let title = |pages, japanese, supplemental| {
            translation_sheets(&lines, pages, japanese, false, supplemental)[0][0]
                .3
                .clone()
        };
        assert_eq!(
            title((11, 11), true, false),
            "機械翻訳  |  原本 12 ページ  |  1"
        );
        assert_eq!(
            title((11, 12), true, false),
            "機械翻訳  |  原本 12～13 ページ  |  1"
        );
        assert_eq!(
            title((11, 11), false, false),
            "Machine translation  |  Original page 12  |  1"
        );
        assert_eq!(
            title((11, 12), false, false),
            "Machine translation  |  Original pages 12-13  |  1"
        );
        assert_eq!(
            title((11, 11), false, true),
            "Supplementary translation  |  Original page 12  |  1"
        );
    }

    #[test]
    #[ignore = "requires the local PDFium library and IPA Gothic font"]
    fn a_paragraph_going_on_at_the_next_page_is_named_and_checked_on_both() {
        // A paragraph spanning two pages must name both pages in its translation.
        let f = tempfile::NamedTempFile::new().unwrap();
        {
            let (_guard, lib) = pdfium().unwrap();
            let mut src = lib.create_new_pdf().unwrap();
            let helvetica = src.fonts_mut().helvetica();
            let pages = [
                vec![
                    "The first paragraph of this short report runs across the whole width of the printed page and",
                    "it goes on line after line, in the same column, until it reaches the bottom of the page where it",
                    "is still not finished, so that the very same sentence has to go on at the very top of the next",
                ],
                vec![
                    "page, where it ends with a full stop.",
                    "Another paragraph starts here.",
                ],
            ];
            for (n, lines) in pages.iter().enumerate() {
                let mut page = src
                    .pages_mut()
                    .create_page_at_end(PdfPagePaperSize::a4())
                    .unwrap();
                for (i, line) in lines.iter().enumerate() {
                    let y = if n == 0 { 120.0 } else { 740.0 } - 14.0 * i as f32;
                    page.objects_mut()
                        .create_text_object(pt(42.0), pt(y), line, helvetica, pt(11.0))
                        .unwrap();
                }
            }
            std::fs::write(f.path(), src.save_to_bytes().unwrap()).unwrap();
        }
        let input = Pdf::read(f.path()).unwrap();
        let units: Vec<_> = input
            .units()
            .iter()
            .map(|u| (u.page, u.text.as_str()))
            .collect();
        assert_eq!(units.len(), 2, "{units:?}");
        assert!(
            units[0]
                .1
                .ends_with("next page, where it ends with a full stop.")
        );
        assert_eq!(units[0].0, 1);
        let untranslated = [None, Some("次の段落の訳文。".to_owned())];
        assert_eq!(input.review_pages(&untranslated, "ja"), [1, 2]);
        // The text of each page of the output, its no-break spaces as spaces.
        let text = |targets: &[Option<String>]| {
            let bytes = input.write(targets, "ja").unwrap();
            let (_guard, lib) = pdfium().unwrap();
            let output = lib.load_pdf_from_byte_slice(&bytes, None).unwrap();
            output
                .pages()
                .iter()
                .map(|page| page.text().unwrap().all().replace('\u{a0}', " "))
                .collect::<Vec<_>>()
        };
        let pages = text(&[
            Some("最初の段落の訳文。".to_owned()),
            Some("次の段落の訳文。".to_owned()),
        ]);
        assert_eq!(pages.len(), 4);
        assert!(pages[1].contains("原本 1～2 ページ"), "{}", pages[1]);
        assert!(pages[1].contains("最初の段落の訳文。"));
        assert!(pages[3].contains("原本 2 ページ"), "{}", pages[3]);
        assert!(!pages[3].contains("原本で確認"));
        // Not translated: the check note on the translation pages of both.
        let pages = text(&untranslated);
        assert!(pages[1].contains("原本で確認"), "{}", pages[1]);
        assert!(pages[3].contains("原本で確認"), "{}", pages[3]);
    }

    #[test]
    #[ignore = "requires the local PDFium library and IPA Gothic font"]
    fn exported_pdf_keeps_every_line_and_places_continuations_before_next_source_page() {
        let f = tempfile::NamedTempFile::new().unwrap();
        {
            let (_guard, lib) = pdfium().unwrap();
            let mut src = lib.create_new_pdf().unwrap();
            let helvetica = src.fonts_mut().helvetica();
            let mut page = src
                .pages_mut()
                .create_page_at_end(PdfPagePaperSize::a4())
                .unwrap();
            page.objects_mut()
                .create_text_object(
                    pt(42.0),
                    pt(720.0),
                    "Original document page",
                    helvetica,
                    pt(11.0),
                )
                .unwrap();
            drop(page);
            src.pages_mut()
                .create_page_at_end(PdfPagePaperSize::a4())
                .unwrap();
            std::fs::write(f.path(), src.save_to_bytes().unwrap()).unwrap();
        }
        let input = Pdf::read(f.path()).unwrap();
        let text = format!(
            "{} EndMarker",
            "Long translation without clipping. ".repeat(170)
        );
        let targets = vec![Some(text)];
        assert_eq!(input.review_pages(&targets, "ja"), [2]);
        let bytes = input.write(&targets, "en").unwrap();
        let (_guard, lib) = pdfium().unwrap();
        let output = lib.load_pdf_from_byte_slice(&bytes, None).unwrap();
        let pages: Vec<_> = output
            .pages()
            .iter()
            .map(|page| page.text().unwrap().all())
            .collect();
        assert!(pages.len() >= 4);
        assert!(pages.first().unwrap().contains("Original document page"));
        assert!(pages[pages.len() - 2].contains("EndMarker"));
        assert!(pages.last().unwrap().trim().is_empty());
        assert_eq!(
            pages
                .iter()
                .map(|p| p.matches("clipping").count())
                .sum::<usize>(),
            170
        );
    }

    #[test]
    fn a_word_hyphenated_at_a_line_end_is_joined_again() {
        let joined = |a: &str, b: &str| {
            let mut s = a.to_owned();
            join_words(&mut s, b);
            s
        };
        assert_eq!(
            joined("This is a compli-", "cated one."),
            "This is a complicated one."
        );
        assert_eq!(joined("a well-", "Known one"), "a well- Known one");
        assert_eq!(joined("日本", "語"), "日本語");
        assert_eq!(joined("1兆6,721", "億円"), "1兆6,721億円");
    }

    // Paragraph boundaries for positioned glyphs.

    /// Glyphs of a line from `x` on baseline `y` (PDF space, upward): CJK a full em wide.
    fn set(text: &str, x: f32, y: f32, size: f32, bold: bool) -> Vec<Glyph> {
        let mut at = x;
        text.chars()
            .map(|ch| {
                let advance = if ch.is_ascii() { 0.5 } else { 1.0 } * size;
                let g = Glyph {
                    ch,
                    x: at,
                    right: at + advance * 0.9,
                    y,
                    size,
                    bold,
                };
                at += advance;
                g
            })
            .collect()
    }

    fn units_of(glyphs: Vec<Glyph>, rhythm: f32) -> Vec<String> {
        group_page(glyphs, 595.0, rhythm)
            .0
            .into_iter()
            .flatten()
            .map(|c| c.text)
            .collect()
    }

    #[test]
    fn a_small_first_glyph_does_not_set_the_size_of_its_line() {
        let mut glyphs = set("¹", 72.0, 700.0, 7.0, false);
        glyphs.extend(set(
            "Note text that goes on and on to the right edge of the page, far to the right and",
            76.0,
            700.0,
            10.0,
            false,
        ));
        glyphs.extend(set("continues below.", 72.0, 688.0, 10.0, false));
        assert_eq!(
            units_of(glyphs, 1.75),
            [
                "¹Note text that goes on and on to the right edge of the page, far to the right and continues below."
            ]
        );
    }

    #[test]
    fn lines_further_apart_join_by_the_documents_rhythm() {
        // Five paragraphs of five lines 1.95 em apart: indented first, short last lines.
        let mut glyphs = Vec::new();
        let mut y = 780.0;
        for _ in 0..5 {
            for line in 0..5 {
                let (x, text) = match line {
                    0 => (
                        100.0,
                        "本文の行送りを確かめる文章で段落の途中なので続きますねえ",
                    ),
                    4 => (88.0, "段落の最後の行ですがここで終わります。"),
                    _ => (
                        88.0,
                        "本文の行送りを確かめる文章で段落の途中なので続きますねえまだ",
                    ),
                };
                glyphs.extend(set(text, x, y, 12.0, false));
                y -= 23.4;
            }
            y -= 12.0;
        }
        let pages = [glyphs.clone()];
        let rhythm = rhythm(pages.iter().map(Vec::as_slice));
        assert!((rhythm / 1.08 - 1.95).abs() < 1e-3, "{rhythm}");
        assert_eq!(units_of(glyphs.clone(), rhythm).len(), 5);
        assert_eq!(units_of(glyphs, 1.75).len(), 25, "1.75 em apart only");
    }

    #[test]
    fn first_line_and_hanging_indents_join_their_paragraph_but_items_stay_apart() {
        let mut glyphs = set(
            "  模型庭園においては、架空42年６月27日の展示棚の交換後、受付、展示担当団",
            95.0,
            700.0,
            12.0,
            false,
        );
        glyphs.extend(set(
            "体や案内係等が用意した模型類の展示・保管に関する関心が高まった。",
            83.0,
            682.0,
            12.0,
            false,
        ));
        glyphs.extend(set(
            "• the first item, whose text goes on to the right edge of the page and",
            72.0,
            600.0,
            12.0,
            false,
        ));
        glyphs.extend(set("then below its mark", 84.0, 586.0, 12.0, false));
        glyphs.extend(set(
            "• the second item, whose text goes on to the right edge of the page",
            72.0,
            572.0,
            12.0,
            false,
        ));
        assert_eq!(
            units_of(glyphs, 1.75),
            [
                "模型庭園においては、架空42年６月27日の展示棚の交換後、受付、展示担当団体や案内係等が用意した模型類の展示・保管に関する関心が高まった。",
                "• the first item, whose text goes on to the right edge of the page and then below its mark",
                "• the second item, whose text goes on to the right edge of the page"
            ]
        );
    }

    #[test]
    fn lines_of_a_narrow_column_join_but_labels_of_one_width_do_not() {
        let column = |texts: &[&str], y0: f32| {
            texts
                .iter()
                .enumerate()
                .flat_map(|(i, t)| set(t, 72.0, y0 - i as f32 * 12.0, 10.0, false))
                .collect::<Vec<_>>()
        };
        // Lines of 216 pt in a page 595 pt wide: short of 71% of the page, at their edge.
        let prose = column(
            &[
                "the column of a paper in two parts",
                "goes on to the edge of its column",
                "line after line before it is over",
                "at a short last line.",
            ],
            700.0,
        );
        assert_eq!(units_of(prose, 1.75).len(), 1);
        let labels = column(
            &[
                "North corridor display",
                "Yellow cabinet section",
                "Adjustable lantern arm",
                "Circular storage rack",
            ],
            700.0,
        );
        assert_eq!(units_of(labels, 1.75).len(), 4);
    }

    #[test]
    fn a_bold_heading_does_not_join_the_text_below_it() {
        let mut glyphs = set(
            "2.3 Folding Paper Lanterns for the Display and Moving In",
            72.0,
            700.0,
            10.0,
            true,
        );
        glyphs.extend(set(
            "Several of the other shelves can also hold lanterns from",
            72.0,
            688.0,
            10.0,
            false,
        ));
        glyphs.extend(set("earlier displays.", 72.0, 676.0, 10.0, false));
        assert_eq!(units_of(glyphs, 1.75).len(), 2);
        // A heading of two bold lines does not either.
        let mut glyphs = set(
            "Materials for arranging the racks, paper models, chalkboards, ID cards, and",
            72.0,
            700.0,
            10.0,
            true,
        );
        glyphs.extend(set("rows of toy boats", 72.0, 688.0, 10.0, true));
        glyphs.extend(set(
            "Several of the other shelves can also hold lanterns from",
            72.0,
            676.0,
            10.0,
            false,
        ));
        assert_eq!(units_of(glyphs, 1.75).len(), 2);
    }

    #[test]
    fn entries_of_a_schedule_each_beginning_with_a_date_stay_apart() {
        // Right-aligned dates start farther left when they have more digits.
        let entries = [
            (
                81.0,
                "5/27：第19回「ともしび館での模型展示に合わせた照明調整について」（紙ほたる工作室）",
            ),
            (
                81.0,
                "8/25：第20回「折り紙模型展示DXの連絡帳～会場で小さな紙模型が並べられる棚へ」（折り星模型室）",
            ),
            (
                72.0,
                "10/27：第21回「紙箱を使った模型整理テクニックのご案内～色付きの台紙の並べ方」（木の棚、紙灯会）",
            ),
            (
                72.0,
                "11/27：第22回「布製見本ラベルを使った展示棚の飾り方～端切れを使い季節に合う分類」（虹色小箱展示班）",
            ),
        ];
        let glyphs = entries
            .iter()
            .enumerate()
            .flat_map(|(i, (x, t))| set(t, *x, 700.0 - i as f32 * 14.0, 10.0, false))
            .collect();
        assert_eq!(units_of(glyphs, 1.75).len(), 4);
        // A paragraph whose wrapped line begins with a year, as its first line does: one.
        let mut glyphs = set(
            "2026年度の予算は前年度から増加し、新たな事業にも重点的に配分されることになったが、",
            72.0,
            700.0,
            10.0,
            false,
        );
        glyphs.extend(set(
            "2027年度も同じ水準を保つ見込みである。",
            72.0,
            686.0,
            10.0,
            false,
        ));
        assert_eq!(units_of(glyphs, 1.75).len(), 1);
    }

    #[test]
    fn a_bold_lead_in_that_ends_inside_the_paragraph_keeps_its_regular_lines() {
        // Regular lines continue the paragraph after the inline bold lead-in ends.
        let mut glyphs = set(
            "Paper plans and illustrated guides for the upper shelves and the two cabinets",
            72.0,
            700.0,
            10.0,
            true,
        );
        glyphs.extend(set(
            "need to be neatly marked and stored by colour. ",
            72.0,
            688.0,
            10.0,
            true,
        ));
        glyphs.extend(set(
            "Paper-XL display stands can fold",
            72.0 + 47.0 * 5.0,
            688.0,
            10.0,
            false,
        ));
        glyphs.extend(set(
            "from fully open to a single flat sheet. We can use them to show thicker card-",
            72.0,
            676.0,
            10.0,
            false,
        ));
        glyphs.extend(set(
            "board, cotton ribbon or any folded paper model.",
            72.0,
            664.0,
            10.0,
            false,
        ));
        assert_eq!(
            units_of(glyphs, 1.75),
            [
                "Paper plans and illustrated guides for the upper shelves and the two cabinets need to be neatly marked and stored by colour. Paper-XL display stands can fold from fully open to a single flat sheet. We can use them to show thicker cardboard, cotton ribbon or any folded paper model."
            ]
        );
    }

    #[test]
    fn table_rows_ending_with_numbers_stay_apart() {
        let mut glyphs = set(
            "LAMP LARGE (Red.+ShadesAB) measured on the upper rack now 84.2 91.1 85.1 91.8",
            60.0,
            700.0,
            10.0,
            false,
        );
        glyphs.extend(set(
            "LAMP LARGE (Blu.+ShadesAB) measured on the lower rack now 86.2 92.2 87.4 93.2",
            60.0,
            688.0,
            10.0,
            false,
        ));
        assert_eq!(units_of(glyphs, 1.75).len(), 2);
        let mut glyphs = set(
            "The model was trained for a long time on the data collected up to the year 2019",
            60.0,
            700.0,
            10.0,
            false,
        );
        glyphs.extend(set("and then evaluated.", 60.0, 688.0, 10.0, false));
        assert_eq!(units_of(glyphs, 1.75).len(), 1);
        // One number before a capitalized word is prose.
        let mut glyphs = set(
            "The small lamp on the upper rack used less power than the one below by over 2.0",
            60.0,
            700.0,
            10.0,
            false,
        );
        glyphs.extend(set("WATTS in the test.", 60.0, 688.0, 10.0, false));
        assert_eq!(units_of(glyphs, 1.75).len(), 1);
        // Contents lines: leaders and a page number.
        let mut glyphs = set(
            "A. Wie baut man eine Papierlaternenstütze? ................................ 2",
            60.0,
            700.0,
            10.0,
            false,
        );
        glyphs.extend(set(
            "B. Laternenentwurf ......................................... 3",
            60.0,
            688.0,
            10.0,
            false,
        ));
        assert_eq!(units_of(glyphs, 1.75).len(), 2);
    }

    #[test]
    fn a_hyphenated_word_and_raised_reference_marks_do_not_split_a_sentence() {
        let mut glyphs = set(
            "This sentence wraps in the middle of a long word near the right edge, compli-",
            72.0,
            700.0,
            10.0,
            false,
        );
        glyphs.extend(set("cated as it is.", 72.0, 688.0, 10.0, false));
        assert_eq!(
            units_of(glyphs, 1.75),
            [
                "This sentence wraps in the middle of a long word near the right edge, complicated as it is."
            ]
        );
        let mut glyphs = set(
            "模型に対して展示の希望があった場合は、ペーパーモデルとして設置でき",
            95.0,
            700.0,
            12.0,
            false,
        );
        // Footnote numbers raised over the next line, apart: a line of their own.
        glyphs.extend(set("8", 203.0, 689.0, 7.0, false));
        glyphs.extend(set("9", 266.0, 689.0, 7.0, false));
        glyphs.extend(set(
            "ない場所を記録することを前提とする。",
            95.0,
            682.0,
            12.0,
            false,
        ));
        assert_eq!(
            units_of(glyphs, 1.75),
            [
                "模型に対して展示の希望があった場合は、ペーパーモデルとして設置できない場所を記録することを前提とする。"
            ]
        );
    }

    #[test]
    fn a_paragraph_going_on_at_the_top_of_the_next_page_is_one_unit() {
        let col = |text: &str, x: f32, bottom: f32, full: bool| Col {
            size: 12.0,
            full,
            ..Col::new(
                text.into(),
                x,
                x + 400.0,
                [x, bottom, x + 400.0, bottom + 12.0],
            )
        };
        let page = |rows: Vec<Col>| {
            (
                rows.into_iter().map(|c| vec![c]).collect(),
                false,
                595.0,
                842.0,
            )
        };
        let mut pages = vec![
            page(vec![
                col(
                    "第１に、棚板の高さに差があり、模型の飾り付けの際に使用する予定の丸い台座",
                    88.0,
                    70.0,
                    true,
                ),
                col("5", 290.0, 30.0, false),
            ]),
            page(vec![
                col("Running header", 88.0, 800.0, false),
                col("はこの棚の右奥の箱にまとめている。", 88.0, 700.0, false),
                col("The next paragraph.", 88.0, 680.0, false),
            ]),
            page(vec![col("Another page begins here.", 88.0, 700.0, false)]),
        ];
        join_across_pages(&mut pages);
        let texts: Vec<Vec<&str>> = pages
            .iter()
            .map(|(rows, ..)| rows.iter().flatten().map(|c| c.text.as_str()).collect())
            .collect();
        assert_eq!(
            texts,
            [
                vec![
                    "第１に、棚板の高さに差があり、模型の飾り付けの際に使用する予定の丸い台座はこの棚の右奥の箱にまとめている。",
                    "5"
                ],
                vec!["Running header", "The next paragraph."],
                vec!["Another page begins here."]
            ]
        );
    }

    #[test]
    fn extracts_rows_in_position_order_and_keeps_amounts_with_units() {
        let glyphs = [
            line("12.5 million yen", 360.0, 710.0),
            line("Net sales", 50.0, 730.0),
            line("1兆6,721億円", 360.0, 730.0),
            line("Operating profit", 50.0, 710.0),
        ]
        .concat();
        let (rows, _) = group_page(glyphs, 595.0, 1.75);
        let texts = rows
            .iter()
            .map(|r| r.iter().map(|c| c.text.as_str()).collect::<Vec<_>>())
            .collect::<Vec<_>>();
        assert_eq!(
            texts,
            [
                vec!["Net sales", "1兆6,721億円"],
                vec!["Operating profit", "12.5 million yen"]
            ]
        );
    }
}

#[cfg(test)]
mod resource_tests {
    use super::*;

    #[test]
    fn candidates_preserve_platform_order_and_explicit_override() {
        let exe = Path::new("/App/Contents/MacOS/program");
        let home = Path::new("/user");
        for macos in [false, true] {
            assert_eq!(
                resource_candidates(
                    exe,
                    macos,
                    &["pdfium.dll"],
                    Some("/missing".into()),
                    Some(home)
                ),
                vec![PathBuf::from("/missing")]
            );
        }
        assert_eq!(
            resource_candidates(
                exe,
                false,
                &["ipafont/ipag.ttf", "pdf-font.ttf"],
                None,
                Some(home)
            ),
            [
                "/App/Contents/MacOS/ipafont/ipag.ttf",
                "/App/Contents/MacOS/pdf-font.ttf"
            ]
            .map(PathBuf::from)
        );
        for library in ["pdfium.dll", "libpdfium.so", "libpdfium.dylib"] {
            assert_eq!(
                resource_candidates(Path::new("/bin/program"), false, &[library], None, None),
                vec![Path::new("/bin").join(library)]
            );
        }
        assert_eq!(
            resource_candidates(
                exe,
                true,
                &["ipafont/ipag.ttf", "pdf-font.ttf"],
                None,
                Some(home)
            ),
            [
                "/App/Contents/Resources/ipafont/ipag.ttf",
                "/App/Contents/Resources/pdf-font.ttf",
                "/App/Contents/MacOS/ipafont/ipag.ttf",
                "/App/Contents/MacOS/pdf-font.ttf",
                "/user/Library/Fonts/ipag.ttf"
            ]
            .map(PathBuf::from)
        );
        assert_eq!(
            resource_candidates(
                Path::new("/bin/program"),
                true,
                &["ipafont/ipag.ttf", "pdf-font.ttf"],
                None,
                Some(home)
            ),
            [
                "/bin/ipafont/ipag.ttf",
                "/bin/pdf-font.ttf",
                "/user/Library/Fonts/ipag.ttf"
            ]
            .map(PathBuf::from)
        );
    }
}
