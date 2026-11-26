//! Implements layout-preserving PDF exports. It removes each translated paragraph's source
//! text from the page and draws its translation in the same box.
//!
//! [`super::layout`] groups the positioned text into paragraphs. Its input, the
//! pieces of text, is made from PDFium's characters: consecutive characters of one text object
//! on one baseline make a piece, as PDF.js's text items do, in points from the page's top left.
//!
//! PDFium's own wrapper cannot tell which text object a character belongs to, so this module
//! calls PDFium's functions directly ([`PdfiumLibraryBindings`]). Every handle is closed by a
//! guard, and the whole of a call runs under the module's PDFium lock ([`super::pdfium`]).

use std::collections::{HashMap, HashSet};
use std::ffi::{c_int, c_uint, c_ulong, c_void};
use std::ptr;

use pdfium_render::prelude::*;

use super::layout::{self, Align, Block, Item, Part, Rect};
use super::{Error, admit, output_font, pdfium};

const PAGEOBJ_TEXT: c_int = 1;
const PAGEOBJ_PATH: c_int = 2;
const PAGEOBJ_IMAGE: c_int = 3;
const PAGEOBJ_FORM: c_int = 5;
const SEGMENT_LINETO: c_int = 0;
const SEGMENT_BEZIERTO: c_int = 1;
const SEGMENT_MOVETO: c_int = 2;
#[cfg(not(feature = "pdf-erase"))]
const FONT_TRUETYPE: c_int = 2;
const NO_INCREMENTAL: FPDF_DWORD = 2;
const RENDER_ANNOTATIONS: c_int = 1;
const TEXT_INVISIBLE: c_int = 3;
const FLAG_FIXED: c_int = 1;
const FLAG_SERIF: c_int = 1 << 1;
const FLAG_ITALIC: c_int = 1 << 6;
const FLAG_FORCE_BOLD: c_int = 1 << 18;
/// No size below this, whatever the smallest scale allows.
const MIN_POINTS: f64 = 4.0;

/// How translations are fitted to their boxes.
#[derive(Clone, Copy, Debug)]
pub struct Options {
    pub family: super::Family,
    /// The smallest scale of a paragraph's size (never below 4 pt).
    pub min_scale: f64,
    /// Whether a paragraph may take lines below its box, up to the next text or drawing.
    pub extend: bool,
    /// Whether same-size, same-boldness, same-color paragraphs share scale and leading.
    pub uniform: bool,
}

impl Default for Options {
    fn default() -> Self {
        Self {
            family: super::Family::Ipa,
            min_scale: 0.5,
            extend: true,
            uniform: true,
        }
    }
}

/// Why a paragraph is kept as drawn although worth translating.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum Retain {
    /// A formula inside the paragraph that stays as drawn.
    Math,
    /// Its text is inside a Form XObject, not on the page itself.
    Nested,
    /// A text object of it also holds text outside it (another paragraph, a formula, a
    /// rotated character).
    Shared,
    /// No translation given, or an empty one.
    Missing,
    /// The font has no glyph for a character of the translation.
    Glyph,
    /// No placement is clear of other content at the permitted sizes and page margins.
    Overflow,
    /// PDFium would not remove a text object of it from the page.
    Erase,
    /// Visible and invisible source runs share a paragraph; preserve both.
    InvisibleSource,
    /// Source paint cannot be copied to a legible filled translation.
    Paint,
    /// The erasure and insertion sets differ; the whole page was restored.
    PlanMismatch,
    /// Cell spacing or surrounding rules indicate a table rather than flowing prose.
    Table,
    /// A thin rule is drawn in the text band of one of its lines (an underline, a table's rule, a
    /// box's side: the page's drawing cannot tell them apart). No path is ever taken off a page,
    /// so the paragraph stays with it; but for a paragraph of one line underlined end to end, whose
    /// translation may be set over the rule, clear of it ([`kept_underline`]).
    Decoration,
}

/// Why a paragraph is not translated at all.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum Skip {
    /// Numbers or figure labels that do not need translation.
    NotWorth,
    /// Invisible text (a scan's text layer).
    Invisible,
    /// Invisible search text superimposed on an image containing the visible lettering.
    ImageText,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum Status {
    /// Erasable and worth translating (in an extraction), or translated (in a write).
    Translate,
    Retain(Retain),
    Skip(Skip),
}

impl Status {
    pub fn name(self) -> &'static str {
        match self {
            Status::Translate => "translate",
            Status::Retain(_) => "retain",
            Status::Skip(_) => "skip",
        }
    }

    pub fn reason(self) -> Option<&'static str> {
        Some(match self {
            Status::Translate => return None,
            Status::Retain(Retain::Decoration) => "unowned-decoration",
            Status::Retain(Retain::Table) => "table",
            Status::Retain(Retain::Math) => "math",
            Status::Retain(Retain::Nested) => "nested",
            Status::Retain(Retain::Shared) => "shared-object",
            Status::Retain(Retain::Missing) => "no-translation",
            Status::Retain(Retain::Glyph) => "missing-glyph",
            Status::Retain(Retain::Overflow) => "overflow",
            Status::Retain(Retain::Erase) => "remove-failed",
            Status::Retain(Retain::InvisibleSource) => "mixed-invisible-source",
            Status::Retain(Retain::Paint) => "unsafe-source-paint",
            Status::Retain(Retain::PlanMismatch) => "plan-mismatch",
            Status::Skip(Skip::NotWorth) => "not-worth",
            Status::Skip(Skip::Invisible) => "invisible",
            Status::Skip(Skip::ImageText) => "image-text",
        })
    }
}

/// A paragraph found on a page.
#[derive(Clone, Debug)]
pub struct Paragraph {
    /// `p<page>-<index>`, both from 1 and 0: stable for the same PDF.
    pub id: String,
    /// From 0.
    pub page: usize,
    pub rect: Rect,
    pub size: f64,
    pub line_height: f64,
    pub align: Align,
    pub lines: usize,
    /// The text to translate, with `□` for a formula.
    pub text: String,
    /// Whether its size is the body size of the document (to a tenth of a point).
    pub body: bool,
    /// Independent of eligibility: also marks retained/shared table cells.
    pub table: bool,
    /// Prose: at least three lines, or sentence punctuation at any font size.
    pub prose: bool,
    /// Short non-prose label, independent of artwork (legacy diagnostic field name).
    pub figure: bool,
    pub status: Status,
    /// The text of each line as PDFium gives it (not normalized), white space removed.
    pub source_lines: Vec<String>,
    /// The rectangles of formulas inside it.
    pub maths: Vec<Rect>,
    /// The rectangle of each line, for developer diagnostics of paragraph boundaries.
    pub line_rects: Vec<Rect>,
}

/// Per-Form diagnostics for the developer probe, not paragraph/page classification.
/// Counts exclude
/// unpainted paths and thin horizontal strokes (including text underlines).
#[derive(Clone, Debug, Default)]
pub struct FormInfo {
    pub depth: usize,
    pub text_objects: usize,
    pub images: usize,
    pub graphic_paths: usize,
    pub graphic_segments: usize,
    // One bounding box per painted object, never one vote per path segment.
    artwork: Vec<Rect>,
    pictures: Vec<Rect>,
}
impl FormInfo {
    /// Coarse probe diagnostics only; never exempts text from coverage or fallback.
    pub fn is_figure(&self) -> bool {
        !self.pictures.is_empty() || self.artwork.len() >= 3
    }
}

#[derive(Clone, Debug)]
pub struct PageInfo {
    pub width: f64,
    pub height: f64,
    /// Whether the page is shown turned (`/Rotate`); its text is read as shown, as PDF.js does.
    pub rotated: bool,
    pub paragraphs: Vec<Paragraph>,
    /// What is on the page in no paragraph, where a translation may not be drawn: lines left
    /// out, text in no piece (on the page or in a Form), and drawings (a path piece by piece).
    pub obstacles: Vec<Rect>,
    pub forms: Vec<FormInfo>,
}

#[derive(Clone, Debug)]
pub struct Analysis {
    /// Most characters by line size on the processed pages, for acceptance measurements.
    pub processed_body_size: f64,
    pub leading_limit: f64,
    pub pages: usize,
    pub body_size: f64,
    pub analyzed: Vec<PageInfo>,
}

/// Ordered fitting operations, also recorded in the probe's paragraph metadata.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum FitStage {
    Original,
    Leading,
    Expand,
    Shrink,
    Overflow,
    Uniform,
}

impl FitStage {
    pub fn name(self) -> &'static str {
        match self {
            Self::Original => "original",
            Self::Leading => "leading",
            Self::Expand => "expand",
            Self::Shrink => "shrink",
            Self::Overflow => "overflow",
            Self::Uniform => "uniform",
        }
    }
}

#[derive(Clone, Debug)]
pub struct FitAttempt {
    pub stage: FitStage,
    pub scale: f64,
    /// Baseline distance in points (not em).
    pub line_height: f64,
    pub fits: bool,
    pub rejection: Option<&'static str>,
    pub candidate: Option<Rect>,
}

/// What became of a paragraph in a write.
#[derive(Clone, Debug)]
pub struct Outcome {
    pub paragraph: Paragraph,
    /// Majority source size, bold flag and RGBA, for same-format diagnostics.
    pub source_style: (String, bool, [u32; 4]),
    pub status: Status,
    pub scale: Option<f64>,
    pub fit_stage: Option<FitStage>,
    pub line_height: Option<f64>,
    pub attempts: Vec<FitAttempt>,
    /// The lines the translation was set in.
    pub drawn_lines: usize,
    /// Per-line occupied rectangles, used by the erased-page pixel audit.
    pub line_rects: Vec<Rect>,
    /// Where the translation was drawn.
    pub drawn: Option<Rect>,
    /// A one-line paragraph whose translation took more lines.
    pub wrapped: bool,
    pub translation: Option<String>,
}

// ---- PDFium handles ----

struct Doc<'a> {
    lib: &'a dyn PdfiumLibraryBindings,
    raw: FPDF_DOCUMENT,
}

impl<'a> Doc<'a> {
    fn load(lib: &'a dyn PdfiumLibraryBindings, bytes: &'a [u8]) -> Result<Self, Error> {
        let raw = lib.FPDF_LoadMemDocument64(bytes, None);
        if raw.is_null() {
            return Err(Error::Pdf(PdfiumError::PdfiumLibraryInternalError(
                PdfiumInternalError::Unknown,
            )));
        }
        Ok(Self { lib, raw })
    }

    fn pages(&self) -> usize {
        self.lib.FPDF_GetPageCount(self.raw).max(0) as usize
    }

    fn page(&self, index: usize) -> Result<Page<'a>, Error> {
        let raw = self.lib.FPDF_LoadPage(self.raw, index as c_int);
        if raw.is_null() {
            return Err(internal());
        }
        Ok(Page { lib: self.lib, raw })
    }

    fn save(&self) -> Result<Vec<u8>, Error> {
        #[repr(C)]
        struct Writer {
            base: FPDF_FILEWRITE,
            out: Vec<u8>,
        }
        unsafe extern "C" fn write_block(
            this: *mut FPDF_FILEWRITE,
            data: *const c_void,
            size: c_ulong,
        ) -> c_int {
            if data.is_null() || size == 0 {
                return 1;
            }
            // SAFETY: `this` points to the whole `Writer` below (`repr(C)`, `base` first), made
            // from a pointer to all of it; PDFium gives `size` bytes at `data`, not null.
            unsafe {
                let writer = &mut *this.cast::<Writer>();
                writer.out.extend_from_slice(std::slice::from_raw_parts(
                    data.cast::<u8>(),
                    size as usize,
                ));
            }
            1
        }
        let mut writer = Writer {
            base: FPDF_FILEWRITE {
                version: 1,
                WriteBlock: Some(write_block),
            },
            out: Vec::new(),
        };
        let ok =
            self.lib
                .FPDF_SaveAsCopy(self.raw, ptr::addr_of_mut!(writer).cast(), NO_INCREMENTAL);
        if ok == 0 {
            return Err(internal());
        }
        Ok(writer.out)
    }
}

impl Drop for Doc<'_> {
    fn drop(&mut self) {
        self.lib.FPDF_CloseDocument(self.raw);
    }
}

struct Page<'a> {
    lib: &'a dyn PdfiumLibraryBindings,
    raw: FPDF_PAGE,
}

impl Drop for Page<'_> {
    fn drop(&mut self) {
        self.lib.FPDF_ClosePage(self.raw);
    }
}

struct TextPage<'a> {
    lib: &'a dyn PdfiumLibraryBindings,
    raw: FPDF_TEXTPAGE,
}

impl Drop for TextPage<'_> {
    fn drop(&mut self) {
        self.lib.FPDFText_ClosePage(self.raw);
    }
}

/// A font loaded into a document, closed when dropped (before the document: declare it after).
struct Font<'a> {
    lib: &'a dyn PdfiumLibraryBindings,
    raw: FPDF_FONT,
}

impl Drop for Font<'_> {
    fn drop(&mut self) {
        if !self.raw.is_null() {
            self.lib.FPDFFont_Close(self.raw);
        }
    }
}

pub(super) fn internal() -> Error {
    Error::Pdf(PdfiumError::PdfiumLibraryInternalError(
        PdfiumInternalError::Unknown,
    ))
}

/// The page as it is shown: its crop box turned by its `/Rotate`. `width` and `height` are the
/// shown page's, and points "from the top left" are the shown page's, as in PDF.js's viewport.
#[derive(Clone, Copy, Debug)]
struct Frame {
    left: f64,
    top: f64,
    /// The crop box's, before turning.
    w: f64,
    h: f64,
    /// Quarter turns clockwise.
    turns: u8,
    width: f64,
    height: f64,
}

impl Frame {
    fn of(page: &Page<'_>) -> Self {
        let mut r = FS_RECTF {
            left: 0.0,
            top: 0.0,
            right: 0.0,
            bottom: 0.0,
        };
        page.lib.FPDF_GetPageBoundingBox(page.raw, &mut r);
        let r = Rect {
            left: f64::from(r.left),
            top: f64::from(r.top),
            right: f64::from(r.right),
            bottom: f64::from(r.bottom),
        };
        let (top, bottom) = (r.top.max(r.bottom), r.top.min(r.bottom));
        let (left, right) = (r.left.min(r.right), r.left.max(r.right));
        let (w, h) = (right - left, top - bottom);
        let turns = page.lib.FPDFPage_GetRotation(page.raw).rem_euclid(4) as u8;
        let (width, height) = if turns % 2 == 1 { (h, w) } else { (w, h) };
        Self {
            left,
            top,
            w,
            h,
            turns,
            width,
            height,
        }
    }

    /// A point of page space, from the shown page's top left.
    fn down(&self, x: f64, y: f64) -> (f64, f64) {
        let (u, v) = (x - self.left, self.top - y);
        match self.turns {
            1 => (self.h - v, u),
            2 => (self.w - u, self.h - v),
            3 => (v, self.w - u),
            _ => (u, v),
        }
    }

    /// A point from the shown page's top left, in page space.
    fn up(&self, x: f64, y: f64) -> (f64, f64) {
        let (u, v) = match self.turns {
            1 => (y, self.h - x),
            2 => (self.w - x, self.h - y),
            3 => (self.w - y, x),
            _ => (x, y),
        };
        (u + self.left, self.top - v)
    }

    /// A direction on the shown page (down positive), in page space (up positive).
    fn direction(&self, dx: f64, dy: f64) -> (f64, f64) {
        let (du, dv) = match self.turns {
            1 => (dy, -dx),
            2 => (-dx, -dy),
            3 => (-dy, dx),
            _ => (dx, dy),
        };
        (du, -dv)
    }

    /// A direction of page space, on the shown page.
    fn shown(&self, dx: f64, dy: f64) -> (f64, f64) {
        let (du, dv) = (dx, -dy);
        match self.turns {
            1 => (-dv, du),
            2 => (-du, -dv),
            3 => (dv, -du),
            _ => (du, dv),
        }
    }

    /// Page-space bounds (left, bottom, right, top), on the shown page.
    fn rect(&self, l: f64, b: f64, r: f64, t: f64) -> Rect {
        let (x1, y1) = self.down(l, b);
        let (x2, y2) = self.down(r, t);
        Rect {
            left: x1.min(x2),
            right: x1.max(x2),
            top: y1.min(y2),
            bottom: y1.max(y2),
        }
    }

    /// A rectangle of the shown page, in page space: (left, bottom, width, height).
    fn page_rect(&self, r: &Rect) -> (f64, f64, f64, f64) {
        let (x1, y1) = self.up(r.left, r.top);
        let (x2, y2) = self.up(r.right, r.bottom);
        (x1.min(x2), y1.min(y2), (x1 - x2).abs(), (y1 - y2).abs())
    }

    /// The matrix of text set left to right on the shown page, from its point `(x, y)`.
    fn text_matrix(&self, x: f64, y: f64) -> [f64; 6] {
        let (x, y) = self.up(x, y);
        let (a, b) = self.direction(1.0, 0.0);
        let (c, d) = self.direction(0.0, -1.0);
        [a, b, c, d, x, y]
    }
}

// ---- Pieces of text from PDFium's characters ----

/// What a piece of text came from, beside the [`Item`] made of it.
#[derive(Clone, Debug)]
struct Origin {
    object: usize,
    /// Its characters as PDFium gives them, white space removed.
    raw: String,
    /// Its characters that are not white space.
    count: usize,
    color: [u32; 4],
    invisible: bool,
    unsafe_paint: bool,
    /// The scale of the text's matrix, without its font size (PDF.js's `textAdvanceScale`).
    scale: f64,
}

struct Scan {
    frame: Frame,
    rotated: bool,
    items: Vec<Item>,
    origins: Vec<Origin>,
    /// The characters (not white space) of each text object on the page, of all its text.
    totals: HashMap<usize, usize>,
    /// The text objects on the page itself (not inside a Form XObject).
    top_level: HashSet<usize>,
    /// The bounds of the text objects, on the page itself and inside its Form XObjects.
    text_bounds: Vec<(usize, Rect)>,
    forms: Vec<FormInfo>,
    /// Where a paragraph's lines may not go: each piece of a path's outline (a line, a curve, a
    /// rectangle's sides each), and images and shadings whole, on the page and inside its Form
    /// XObjects. A box drawn around a paragraph is its four sides, which it may go down to.
    drawings: Vec<Rect>,
    /// Images can contain unextractable lettering (for example a title shadow).
    pictures: Vec<Rect>,
    /// Entire top-level paths consisting solely of thin horizontal strokes (underlines, a table's
    /// rules, a box's sides: never taken off the page, whatever they are).
    decorations: Vec<(usize, Rect)>,
    decoration_colors: HashMap<usize, [u32; 4]>,
    links: Vec<Rect>,
}

/// A PDF matrix `[a b c d e f]`: `(x, y)` to `(a x + c y + e, b x + d y + f)`.
type Matrix = [f64; 6];

const IDENTITY: Matrix = [1.0, 0.0, 0.0, 1.0, 0.0, 0.0];

fn apply(m: &Matrix, x: f64, y: f64) -> (f64, f64) {
    (m[0] * x + m[2] * y + m[4], m[1] * x + m[3] * y + m[5])
}

/// `inner`, then `outer`.
fn then(inner: &Matrix, outer: &Matrix) -> Matrix {
    let (e, f) = apply(outer, inner[4], inner[5]);
    [
        outer[0] * inner[0] + outer[2] * inner[1],
        outer[1] * inner[0] + outer[3] * inner[1],
        outer[0] * inner[2] + outer[2] * inner[3],
        outer[1] * inner[2] + outer[3] * inner[3],
        e,
        f,
    ]
}

/// The page-space bounds `(left, bottom, right, top)` of points, through `m`.
fn bounds_of(m: &Matrix, points: &[(f64, f64)]) -> (f64, f64, f64, f64) {
    points.iter().fold(
        (
            f64::INFINITY,
            f64::INFINITY,
            f64::NEG_INFINITY,
            f64::NEG_INFINITY,
        ),
        |(l, b, r, t), &(x, y)| {
            let (x, y) = apply(m, x, y);
            (l.min(x), b.min(y), r.max(x), t.max(y))
        },
    )
}

fn matrix_of(lib: &dyn PdfiumLibraryBindings, object: FPDF_PAGEOBJECT) -> Matrix {
    let mut m = FS_MATRIX {
        a: 1.0,
        b: 0.0,
        c: 0.0,
        d: 1.0,
        e: 0.0,
        f: 0.0,
    };
    if lib.FPDFPageObj_GetMatrix(object, &mut m) == 0 {
        return IDENTITY;
    }
    [m.a, m.b, m.c, m.d, m.e, m.f].map(f64::from)
}

/// The obstacles of a page object, `outer` taking its space (a Form's) to the page's: text to
/// `text_bounds`, the rest to `drawings`, a Form's of what is in it.
fn obstacles(
    lib: &dyn PdfiumLibraryBindings,
    object: FPDF_PAGEOBJECT,
    outer: &Matrix,
    depth: usize,
    out: &mut Scan,
) {
    let frame = out.frame;
    let kind = lib.FPDFPageObj_GetType(object);
    if kind == PAGEOBJ_FORM && depth < 16 {
        let inner = then(&matrix_of(lib, object), outer);
        let index = out.forms.len();
        out.forms.push(FormInfo {
            depth,
            ..FormInfo::default()
        });
        for k in 0..lib.FPDFFormObj_CountObjects(object).max(0) {
            let child = lib.FPDFFormObj_GetObject(object, k as c_ulong);
            if child.is_null() {
                continue;
            }
            let child_kind = lib.FPDFPageObj_GetType(child);
            if child_kind == PAGEOBJ_TEXT {
                out.forms[index].text_objects += 1;
            }
            let start = out.drawings.len();
            let child_form = out.forms.len();
            obstacles(lib, child, &inner, depth + 1, out);
            if child_kind == PAGEOBJ_IMAGE {
                out.forms[index].images += 1;
                out.forms[index]
                    .pictures
                    .extend_from_slice(&out.drawings[start..]);
            } else if child_kind == PAGEOBJ_FORM {
                if let Some(evidence) = out.forms.get(child_form).cloned() {
                    out.forms[index].images += evidence.images;
                    out.forms[index].graphic_paths += evidence.graphic_paths;
                    out.forms[index].graphic_segments += evidence.graphic_segments;
                    out.forms[index].artwork.extend(evidence.artwork);
                    out.forms[index].pictures.extend(evidence.pictures);
                }
            } else if child_kind == PAGEOBJ_PATH {
                let pieces = &out.drawings[start..];
                let top = pieces.iter().map(|r| r.top).fold(f64::INFINITY, f64::min);
                let bottom = pieces
                    .iter()
                    .map(|r| r.bottom)
                    .fold(f64::NEG_INFINITY, f64::max);
                let (mut fill, mut stroke) = (0, 0);
                if lib.FPDFPath_GetDrawMode(child, &mut fill, &mut stroke) != 0
                    && (fill != 0 || stroke != 0)
                    && bottom - top > 1.5
                {
                    let left = pieces.iter().map(|r| r.left).fold(f64::INFINITY, f64::min);
                    let right = pieces
                        .iter()
                        .map(|r| r.right)
                        .fold(f64::NEG_INFINITY, f64::max);
                    // Outline rectangles are frames, not four graph objects.
                    let rectangle = (4..=5).contains(&pieces.len())
                        && pieces.iter().all(|r| {
                            ((r.left - left).abs() < 0.01
                                && (r.right - right).abs() < 0.01
                                && ((r.top - top).abs() < 0.01 || (r.bottom - bottom).abs() < 0.01))
                                || ((r.top - top).abs() < 0.01
                                    && (r.bottom - bottom).abs() < 0.01
                                    && ((r.left - left).abs() < 0.01
                                        || (r.right - right).abs() < 0.01))
                                || (r.right - r.left <= 1.5 && r.bottom - r.top <= 1.5)
                        });
                    // Frames, squares and horizontal background strips cannot
                    // establish a graph. Narrow vertical bars may contribute.
                    let decoration =
                        rectangle && (fill == 0 || right - left >= (bottom - top) * 0.5);
                    if !decoration {
                        out.forms[index].artwork.push(Rect {
                            left,
                            right,
                            top,
                            bottom,
                        });
                    }
                    out.forms[index].graphic_paths += 1;
                    out.forms[index].graphic_segments += pieces
                        .iter()
                        .filter(|r| r.right - r.left > 1.5 || r.bottom - r.top > 1.5)
                        .count();
                }
            }
        }
        return;
    }
    if kind == PAGEOBJ_PATH && path_pieces(lib, object, outer, out) {
        if depth != 0 {
            out.decorations.retain(|(o, _)| *o != object as usize);
        }
        return;
    }
    let (mut l, mut b, mut r, mut t) = (0.0f32, 0.0f32, 0.0f32, 0.0f32);
    if lib.FPDFPageObj_GetBounds(object, &mut l, &mut b, &mut r, &mut t) == 0 {
        return;
    }
    let (l, b, r, t) = bounds_of(
        outer,
        &[
            (f64::from(l), f64::from(b)),
            (f64::from(r), f64::from(t)),
            (f64::from(l), f64::from(t)),
            (f64::from(r), f64::from(b)),
        ],
    );
    let rect = frame.rect(l, b, r, t);
    if kind == PAGEOBJ_IMAGE {
        out.pictures.push(rect);
    }
    if kind == PAGEOBJ_TEXT {
        let mode = lib.FPDFTextObj_GetTextRenderMode(object);
        let (mut r, mut g, mut b, mut a) = (0, 0, 0, 255);
        lib.FPDFPageObj_GetFillColor(object, &mut r, &mut g, &mut b, &mut a);
        if matches!(mode, 3 | 7) || (matches!(mode, 0 | 4) && a <= 2) {
            return;
        }
        out.text_bounds.push((object as usize, rect));
    } else {
        out.drawings.push(rect);
    }
}

/// A path's outline, piece by piece (a line between two points, a curve within its control
/// points), widened by half its stroke. False where its segments cannot be read.
fn path_pieces(
    lib: &dyn PdfiumLibraryBindings,
    object: FPDF_PAGEOBJECT,
    outer: &Matrix,
    out: &mut Scan,
) -> bool {
    let n = lib.FPDFPath_CountSegments(object);
    if n <= 0 {
        return false;
    }
    let m = then(&matrix_of(lib, object), outer);
    let mut width = 0.0f32;
    let half = if lib.FPDFPageObj_GetStrokeWidth(object, &mut width) != 0 {
        f64::from(width) / 2.0 * (m[0] * m[3] - m[1] * m[2]).abs().sqrt()
    } else {
        0.0
    };
    let frame = out.frame;
    let mut pieces = Vec::new();
    let (mut start, mut at) = ((0.0, 0.0), (0.0, 0.0));
    let mut curve: Vec<(f64, f64)> = Vec::new();
    for k in 0..n {
        let seg = lib.FPDFPath_GetPathSegment(object, k);
        let (mut x, mut y) = (0.0f32, 0.0f32);
        if seg.is_null() || lib.FPDFPathSegment_GetPoint(seg, &mut x, &mut y) == 0 {
            return false;
        }
        let p = (f64::from(x), f64::from(y));
        match lib.FPDFPathSegment_GetType(seg) {
            SEGMENT_MOVETO => {
                start = p;
                at = p;
                curve.clear();
                continue;
            }
            SEGMENT_LINETO => {
                pieces.push(bounds_of(&m, &[at, p]));
            }
            SEGMENT_BEZIERTO => {
                curve.push(p);
                if curve.len() < 3 {
                    continue;
                }
                let mut points = vec![at];
                points.append(&mut curve);
                pieces.push(bounds_of(&m, &points));
            }
            _ => return false,
        }
        at = p;
        if lib.FPDFPathSegment_GetClose(seg) != 0 {
            pieces.push(bounds_of(&m, &[at, start]));
            at = start;
        }
    }
    // A filled thin rectangle (Word's underline) or a single horizontal
    // stroke is removable as a whole. A table's multi-line path is not.
    let (mut fill, mut stroke) = (0, 0);
    if lib.FPDFPath_GetDrawMode(object, &mut fill, &mut stroke) != 0 {
        let l = pieces.iter().map(|p| p.0).fold(f64::INFINITY, f64::min);
        let b = pieces.iter().map(|p| p.1).fold(f64::INFINITY, f64::min);
        let r = pieces.iter().map(|p| p.2).fold(f64::NEG_INFINITY, f64::max);
        let t = pieces.iter().map(|p| p.3).fold(f64::NEG_INFINITY, f64::max);
        let pad = if stroke != 0 { half } else { 0.0 };
        let rect = frame.rect(l - pad, b - pad, r + pad, t + pad);
        if outer == &IDENTITY
            && !pieces.is_empty()
            && pieces.len() <= 5
            && (fill != 0 || pieces.len() == 1)
            && pieces.iter().all(|(x0, y0, x1, y1)| {
                ((x1 - x0).abs() < 0.01 && ((x0 - l).abs() < 0.01 || (x0 - r).abs() < 0.01))
                    || ((y1 - y0).abs() < 0.01 && ((y0 - b).abs() < 0.01 || (y0 - t).abs() < 0.01))
            })
            && rect.bottom - rect.top <= 1.0
            && rect.right - rect.left > 2.0
        {
            let (mut red, mut green, mut blue, mut alpha) = (0, 0, 0, 0);
            let ok = if fill != 0 {
                lib.FPDFPageObj_GetFillColor(object, &mut red, &mut green, &mut blue, &mut alpha)
            } else {
                lib.FPDFPageObj_GetStrokeColor(object, &mut red, &mut green, &mut blue, &mut alpha)
            };
            if ok != 0 && (fill == 0 || stroke == 0) {
                out.decoration_colors
                    .insert(object as usize, [red, green, blue, alpha]);
            }
            out.decorations.push((object as usize, rect));
            // Use the actual painted extent, not an unused stroke's width.
            out.drawings.push(rect);
            return true;
        }
    }
    for (l, b, r, t) in pieces {
        out.drawings
            .push(frame.rect(l - half, b - half, r + half, t + half));
    }
    true
}

#[derive(Clone)]
struct FontInfo {
    name: String,
    flags: c_int,
    ascent: f64,
    descent: f64,
}

struct Building {
    object: usize,
    x: f64,
    y: f64,
    right: f64,
    size: f64,
    raw: String,
    str: String,
    count: usize,
    weight: c_int,
    scale: f64,
}

fn scan(lib: &dyn PdfiumLibraryBindings, page: &Page<'_>) -> Scan {
    let frame = Frame::of(page);
    let mut out = Scan {
        frame,
        rotated: frame.turns != 0,
        items: Vec::new(),
        origins: Vec::new(),
        totals: HashMap::new(),
        top_level: HashSet::new(),
        text_bounds: Vec::new(),
        forms: Vec::new(),
        drawings: Vec::new(),
        pictures: Vec::new(),
        decorations: Vec::new(),
        decoration_colors: HashMap::new(),
        links: Vec::new(),
    };
    for i in 0..lib.FPDFPage_CountObjects(page.raw) {
        let object = lib.FPDFPage_GetObject(page.raw, i);
        if object.is_null() {
            continue;
        }
        if lib.FPDFPageObj_GetType(object) == PAGEOBJ_TEXT {
            out.top_level.insert(object as usize);
        }
        obstacles(lib, object, &IDENTITY, 0, &mut out);
    }
    for i in 0..lib.FPDFPage_GetAnnotCount(page.raw) {
        let annot = lib.FPDFPage_GetAnnot(page.raw, i);
        if annot.is_null() {
            continue;
        }
        let mut r = FS_RECTF {
            left: 0.0,
            bottom: 0.0,
            right: 0.0,
            top: 0.0,
        };
        if lib.FPDFAnnot_GetRect(annot, &mut r) != 0 {
            let rect = frame.rect(r.left.into(), r.bottom.into(), r.right.into(), r.top.into());
            if lib.FPDFAnnot_GetSubtype(annot) == 2 {
                out.links.push(rect);
            } else {
                out.drawings.push(rect);
            }
        }
        lib.FPDFPage_CloseAnnot(annot);
    }
    let text = lib.FPDFText_LoadPage(page.raw);
    if text.is_null() {
        return out;
    }
    let text = TextPage { lib, raw: text };
    let mut fonts: HashMap<usize, FontInfo> = HashMap::new();
    let mut current: Option<Building> = None;
    let mut space = false;
    let n = lib.FPDFText_CountChars(text.raw);
    for i in 0..n {
        let ch = char::from_u32(lib.FPDFText_GetUnicode(text.raw, i));
        if lib.FPDFText_IsGenerated(text.raw, i) == 1 {
            if ch == Some(' ') {
                space = true;
            } else {
                finish(&mut out, &mut current, &fonts, lib);
            }
            continue;
        }
        let object = lib.FPDFText_GetTextObject(text.raw, i);
        if object.is_null() {
            continue;
        }
        let key = object as usize;
        let ch = match ch {
            Some(c) if c.is_whitespace() => {
                if current.as_ref().is_some_and(|c| c.object == key) {
                    space = true;
                }
                continue;
            }
            // PDFium gives a hyphen at the end of a line as U+0002; PDF.js gives `-`, which
            // joins the word again (`partsOf`).
            Some('\u{2}') => '-',
            Some(c) if !c.is_control() => c,
            // A glyph with no character: text of its object that no piece has, so that the
            // object is not removed with a paragraph (it is counted in its total, not in the
            // paragraph's: `Retain::Shared`).
            _ => {
                *out.totals.entry(key).or_default() += 1;
                continue;
            }
        };
        *out.totals.entry(key).or_default() += 1;
        let mut m = FS_MATRIX {
            a: 0.0,
            b: 0.0,
            c: 0.0,
            d: 0.0,
            e: 0.0,
            f: 0.0,
        };
        lib.FPDFText_GetMatrix(text.raw, i, &mut m);
        // The size as PDF.js gives it: `Tf` times the scale of the text's matrix.
        let m = [m.a, m.b, m.c, m.d].map(f64::from);
        let size = lib.FPDFText_GetFontSize(text.raw, i) * m[2].hypot(m[3]);
        // Rotated, mirrored, or tiny text stays as drawn.
        let (da, db) = frame.shown(m[0], m[1]);
        if da <= 0.0 || db.abs() > 0.05 * da || size.is_nan() || size < 2.0 {
            finish(&mut out, &mut current, &fonts, lib);
            continue;
        }
        let (mut ox, mut oy) = (0.0, 0.0);
        let mut bx = FS_RECTF {
            left: 0.0,
            top: 0.0,
            right: 0.0,
            bottom: 0.0,
        };
        if lib.FPDFText_GetCharOrigin(text.raw, i, &mut ox, &mut oy) == 0
            || lib.FPDFText_GetLooseCharBox(text.raw, i, &mut bx) == 0
        {
            finish(&mut out, &mut current, &fonts, lib);
            continue;
        }
        let (x, y) = frame.down(ox, oy);
        let right = frame
            .rect(
                f64::from(bx.left),
                f64::from(bx.bottom),
                f64::from(bx.right),
                f64::from(bx.top),
            )
            .right
            .max(x);
        // As PDF.js makes its items: one goes on over a gap of up to 0.6 em, and ends where
        // the text goes back more than 0.2 em or leaves its line.
        let joins = current.as_ref().is_some_and(|c| {
            c.object == key
                && (y - c.y).abs() < 0.25 * c.size.max(size)
                && x > c.right - 0.2 * size
                && x - c.right <= 0.6 * size
        });
        if !joins {
            finish(&mut out, &mut current, &fonts, lib);
            gap(&mut out, x, y, size);
            let scale = m[0].hypot(m[1]);
            fonts
                .entry(key_font(lib, object))
                .or_insert_with(|| font_info(lib, object, size));
            current = Some(Building {
                object: key,
                x,
                y,
                right,
                size,
                raw: String::new(),
                str: String::new(),
                count: 0,
                weight: lib.FPDFText_GetFontWeight(text.raw, i),
                scale,
            });
            space = false;
        }
        let c = current.as_mut().unwrap();
        if space {
            c.str.push(' ');
            space = false;
        }
        c.raw.push(ch);
        c.str.push(ch);
        c.count += 1;
        c.right = c.right.max(right);
    }
    finish(&mut out, &mut current, &fonts, lib);
    drop(text);
    out
}

/// PDF.js puts a piece of white space between pieces of one line more than 0.6 em apart
/// (`pushWhitespace`). Lines of a table of contents or a table are joined over it, so it is made
/// here too, from no text object (`object` 0). Its width is the gap's in the text's own units:
/// the gap divided by the scale of the text's matrix, which is the gap itself only where that
/// scale is 1 (as PDF.js has it; where the size is in the matrix, the piece falls short).
fn gap(out: &mut Scan, x: f64, y: f64, size: f64) {
    let Some(prev) = out.items.last() else {
        return;
    };
    let right = prev.x + prev.w;
    let scale = out.origins[prev.id].scale.max(f64::MIN_POSITIVE);
    if prev.str.trim().is_empty() || (y - prev.y).abs() > prev.size || x - right <= 0.6 * size {
        return;
    }
    let item = Item {
        str: " ".into(),
        x: right,
        w: (x - right) / scale,
        math: false,
        id: out.items.len(),
        ..prev.clone()
    };
    out.items.push(item);
    out.origins.push(Origin {
        object: 0,
        raw: String::new(),
        count: 0,
        color: [0, 0, 0, 255],
        invisible: false,
        unsafe_paint: false,
        scale,
    });
}

fn key_font(lib: &dyn PdfiumLibraryBindings, object: FPDF_PAGEOBJECT) -> usize {
    lib.FPDFTextObj_GetFont(object) as usize
}

fn font_info(lib: &dyn PdfiumLibraryBindings, object: FPDF_PAGEOBJECT, size: f64) -> FontInfo {
    let font = lib.FPDFTextObj_GetFont(object);
    let mut name = String::new();
    if !font.is_null() {
        let len = lib.FPDFFont_GetBaseFontName(font, ptr::null_mut(), 0);
        if len > 0 {
            let mut buf = vec![0u8; len];
            lib.FPDFFont_GetBaseFontName(font, buf.as_mut_ptr().cast(), len);
            buf.truncate(buf.iter().position(|&b| b == 0).unwrap_or(buf.len()));
            name = String::from_utf8_lossy(&buf).into_owned();
        }
    }
    let (mut ascent, mut descent) = (0.0f32, 0.0f32);
    let points = size as f32;
    let (a, d) = if font.is_null() {
        (0.8, 0.2)
    } else {
        let a = if lib.FPDFFont_GetAscent(font, points, &mut ascent) != 0 && ascent > 0.0 {
            f64::from(ascent) / size
        } else {
            0.8
        };
        let d = if lib.FPDFFont_GetDescent(font, points, &mut descent) != 0 && descent < 0.0 {
            -f64::from(descent) / size
        } else {
            0.2
        };
        (a, d)
    };
    FontInfo {
        name,
        flags: if font.is_null() {
            0
        } else {
            lib.FPDFFont_GetFlags(font)
        },
        // Clamp font metrics before estimating text bounds.
        ascent: a.clamp(0.6, 1.1),
        descent: d.clamp(0.1, 0.4),
    }
}

fn finish(
    out: &mut Scan,
    current: &mut Option<Building>,
    fonts: &HashMap<usize, FontInfo>,
    lib: &dyn PdfiumLibraryBindings,
) {
    let Some(b) = current.take() else {
        return;
    };
    let object = b.object as FPDF_PAGEOBJECT;
    let font = fonts
        .get(&key_font(lib, object))
        .cloned()
        .unwrap_or(FontInfo {
            name: String::new(),
            flags: 0,
            ascent: 0.8,
            descent: 0.2,
        });
    let s = layout::normalize(&b.str);
    let flags = font.flags;
    let kind = layout::font_kind(
        &font.name,
        layout::FontFlags {
            mono: Some(flags & FLAG_FIXED != 0),
            serif: Some(flags & FLAG_SERIF != 0),
            bold: Some(flags & FLAG_FORCE_BOLD != 0 || b.weight >= 700),
            italic: Some(flags & FLAG_ITALIC != 0),
        },
    );
    let (mut r, mut g, mut bl, mut a) = (0, 0, 0, 255);
    lib.FPDFPageObj_GetFillColor(object, &mut r, &mut g, &mut bl, &mut a);
    let mode = lib.FPDFTextObj_GetTextRenderMode(object);
    let (mut sr, mut sg, mut sb, mut sa) = (0, 0, 0, 255);
    lib.FPDFPageObj_GetStrokeColor(object, &mut sr, &mut sg, &mut sb, &mut sa);
    // PDF render modes 0/4 fill, 1/5 stroke, 2/6 both, 3/7 paint nothing.
    let invisible = match mode {
        TEXT_INVISIBLE | 7 => true,
        1 | 5 => sa <= 2,
        2 | 6 => a <= 2 && sa <= 2,
        _ => a <= 2,
    };
    let id = out.items.len();
    out.items.push(Item {
        math: layout::is_math(&font.name, &s),
        str: s,
        x: b.x,
        y: b.y,
        w: b.right - b.x,
        size: b.size,
        top: b.y - font.ascent * b.size,
        bottom: b.y + font.descent * b.size,
        font: font.name,
        bold: kind.bold,
        italic: kind.italic,
        mono: kind.mono,
        serif: kind.serif,
        id,
    });
    out.origins.push(Origin {
        object: b.object,
        raw: b.raw,
        count: b.count,
        color: [r, g, bl, a],
        invisible,
        unsafe_paint: matches!(mode, 1 | 5) || a <= 30,
        scale: b.scale,
    });
}

// ---- Paragraphs ----

/// A page's paragraphs, with what they came from.
struct Found {
    scan: Scan,
    blocks: Vec<Block>,
    /// The lines in no paragraph (formulas, empty), as obstacles.
    others: Vec<Rect>,
    paragraphs: Vec<Paragraph>,
    /// The thin rules of the page, piece by piece joined.
    chains: Vec<Chain>,
    /// Paragraphs of one line underlined end to end by a rule (left as it is drawn, as every
    /// rule is): their translation may be set over it, in one line, where it is clear of it
    /// ([`kept_underline`]).
    kept_underlines: HashSet<usize>,
}

/// Thin rules along one height, drawn as pieces that touch or overlap end to end, in one color
/// (Word's rectangles under the runs of a link, ulem's lines under words and spaces): what may
/// be one underline, or one rule of a table.
#[derive(Clone, Debug)]
struct Chain {
    /// Into `Scan::decorations`.
    pieces: Vec<usize>,
    /// All of them.
    rect: Rect,
    color: Option<[u32; 4]>,
    /// The paragraph whose line it runs along, end to end ([`classify_decoration`]).
    along: Option<usize>,
}

fn body_size(scans: &[Vec<layout::Line>]) -> f64 {
    // Find the dominant font size by counting text in non-formula lines.
    let mut sizes: Vec<(String, usize, f64)> = Vec::new();
    for lines in scans {
        for ln in lines.iter().filter(|l| !l.math) {
            // Group sizes rounded to one decimal place.
            let key = layout::fixed1(ln.size);
            match sizes.iter_mut().find(|(k, _, _)| *k == key) {
                Some((_, n, _)) => *n += ln.chars,
                None => sizes.push((key, ln.chars, ln.size)),
            }
        }
    }
    let mut best: Option<&(String, usize, f64)> = None;
    for s in &sizes {
        if best.is_none_or(|b| s.1 > b.1) {
            best = Some(s);
        }
    }
    best.map_or(10.0, |(k, _, size)| k.parse().unwrap_or(*size))
}

#[cfg(test)]
fn find(scan: Scan, page: usize, body: f64) -> Found {
    let leading = layout::leading_limit(&[layout::lines_of(scan.items.clone())]);
    find_with_leading(scan, page, body, leading)
}

/// Table evidence stays separate from worth-translating: no text is dropped.
fn table_like(block: &Block, scan: &Scan) -> bool {
    let size = block.size;
    let mut gaps = 0;
    let mut numeric = 0;
    let mut chars = 0;
    for line in &block.lines {
        let mut items: Vec<_> = line
            .items
            .iter()
            .filter(|it| !it.str.trim().is_empty())
            .collect();
        items.sort_by(|a, b| a.x.total_cmp(&b.x));
        gaps += items
            .windows(2)
            .filter(|p| p[1].x - p[0].x - p[0].w > 1.5 * size)
            .count();
        for c in items
            .iter()
            .flat_map(|it| it.str.chars())
            .filter(|c| !c.is_whitespace())
        {
            chars += 1;
            numeric += usize::from(c.is_numeric());
        }
    }
    let side = |left: bool| {
        scan.drawings.iter().any(|r| {
            r.right - r.left <= 1.5
                && r.top <= block.top
                && r.bottom >= block.bottom
                && if left {
                    r.right <= block.left && block.left - r.right < 2.0 * size
                } else {
                    r.left >= block.right && r.left - block.right < 2.0 * size
                }
        })
    };
    let ruled = scan.drawings.iter().any(|top| {
        top.bottom - top.top <= 1.5
            && top.right - top.left > 8.0 * size
            && top.top <= block.top
            && block.top - top.bottom < 3.0 * size
            && top.left <= block.left + size
            && top.right >= block.right - size
            && scan.drawings.iter().any(|bottom| {
                bottom.bottom - bottom.top <= 1.5
                    && bottom.left <= block.left + size
                    && bottom.right >= block.right - size
                    && bottom.top >= block.bottom
                    && bottom.top - top.bottom < 15.0 * size
                    && scan
                        .items
                        .iter()
                        .filter(|it| {
                            it.y >= top.bottom
                                && it.y <= bottom.top
                                && it.x >= top.left
                                && it.x + it.w <= top.right + size
                        })
                        .flat_map(|it| it.str.chars())
                        .filter(|c| c.is_numeric())
                        .count()
                        >= 8
            })
    });
    (ruled && gaps > 0)
        || (block.right - block.left < 20.0 * size && side(true) && side(false))
        || (gaps >= 2 && numeric * 10 >= chars)
        || (gaps >= block.lines.len().max(2) && numeric > 0)
}

/// Preserve list/contents rows instead of joining independently meaningful lines.
fn independent_list_lines(block: &Block) -> bool {
    let numbered: Vec<_> = block
        .lines
        .iter()
        .filter_map(|line| {
            let text = line
                .items
                .iter()
                .map(|it| it.str.as_str())
                .collect::<Vec<_>>()
                .join(" ");
            let text = text.trim();
            let page_number = text
                .split_whitespace()
                .last()
                .is_some_and(|w| !w.is_empty() && w.chars().all(|c| c.is_ascii_digit()));
            if !page_number {
                return None;
            }
            let prefix = text.trim_end_matches(|c: char| c.is_ascii_digit() || c.is_whitespace());
            let leader =
                prefix.ends_with("...") || prefix.ends_with('…') || prefix.ends_with("．．");
            let mut items: Vec<_> = line
                .items
                .iter()
                .filter(|it| !it.str.trim().is_empty())
                .collect();
            items.sort_by(|a, b| a.x.total_cmp(&b.x));
            let wide_gap = items.windows(2).any(|pair| {
                pair[1].x - (pair[0].x + pair[0].w) >= 2.0 * block.size
                    && !pair[1].str.trim().is_empty()
                    && pair[1].str.trim().chars().all(|c| c.is_ascii_digit())
            });
            Some((line.right, leader || wide_gap))
        })
        .collect();
    let independent = numbered.iter().filter(|(_, leader)| *leader).count();
    independent >= 1
}

/// Text shape alone determines coverage, regardless of Forms or nearby artwork.
fn prose_block(block: &Block) -> bool {
    let text = block
        .lines
        .iter()
        .flat_map(|l| &l.items)
        .map(|it| it.str.as_str())
        .collect::<Vec<_>>()
        .join(" ");
    block.lines.len() >= 3
        || text
            .trim()
            .trim_end_matches([')', '）', '"', '”'])
            .ends_with(['。', '.', '!', '?', '！', '？'])
}

fn label_block(block: &Block) -> bool {
    !prose_block(block)
        && block.lines.len() <= 2
        && block
            .lines
            .iter()
            .flat_map(|l| &l.items)
            .flat_map(|it| it.str.chars())
            .filter(|c| !c.is_whitespace())
            .count()
            <= 80
}

impl Paragraph {
    /// Includes formula-bearing prose (formula placeholders stay in the text),
    /// unplaceable titles and tables; excludes non-linguistic/small labels.
    pub fn translation_candidate(&self) -> bool {
        self.status != Status::Skip(Skip::NotWorth)
    }
}

fn find_with_leading(scan: Scan, page: usize, body: f64, leading: f64) -> Found {
    let width = scan.frame.width;
    let lines = layout::lines_of(scan.items.clone());
    let others = lines
        .iter()
        .filter(|l| layout::left_out(l))
        .map(|l| Rect {
            left: l.left,
            right: l.right,
            top: l.top,
            bottom: l.bottom,
        })
        .collect();
    let blocks = layout::blocks_with_leading(lines, width, leading);
    let mut paragraphs = Vec::new();
    for (index, block) in blocks.iter().enumerate() {
        let parts = layout::parts_of(block);
        let items = || block.lines.iter().flat_map(|l| &l.items);
        let status = if !layout::worth_translating(&parts, block, body) {
            Status::Skip(Skip::NotWorth)
        } else if items().any(|it| scan.origins[it.id].invisible) {
            let r = Rect {
                left: block.left,
                right: block.right,
                top: block.top,
                bottom: block.bottom,
            };
            if !items().all(|it| scan.origins[it.id].invisible || scan.origins[it.id].object == 0) {
                Status::Retain(Retain::InvisibleSource)
            } else if scan
                .pictures
                .iter()
                .any(|picture| meet(&r, picture).is_some())
            {
                Status::Skip(Skip::ImageText)
            } else if items()
                .all(|it| scan.origins[it.id].invisible || scan.origins[it.id].object == 0)
            {
                Status::Skip(Skip::Invisible)
            } else {
                Status::Retain(Retain::InvisibleSource)
            }
        } else if parts.iter().any(|p| matches!(p, Part::Math { .. })) {
            Status::Retain(Retain::Math)
        } else if items().any(|it| {
            let object = scan.origins[it.id].object;
            object != 0 && !scan.top_level.contains(&object)
        }) {
            Status::Retain(Retain::Nested)
        } else {
            let mut used: HashMap<usize, usize> = HashMap::new();
            for it in items() {
                let o = &scan.origins[it.id];
                if o.object != 0 {
                    *used.entry(o.object).or_default() += o.count;
                }
            }
            if used
                .iter()
                .any(|(object, n)| scan.totals.get(object).copied().unwrap_or(0) > *n)
            {
                Status::Retain(Retain::Shared)
            } else {
                Status::Translate
            }
        };
        paragraphs.push(Paragraph {
            id: format!("p{}-{}", page + 1, index),
            page,
            rect: Rect {
                left: block.left,
                right: block.right,
                top: block.top,
                bottom: block.bottom,
            },
            size: block.size,
            line_height: layout::line_height(block),
            align: layout::alignment(block, width),
            lines: block.lines.len(),
            text: layout::parts_text(&parts, "□").trim().to_owned(),
            // The body size as counted (to a tenth of a point), not sizes near it: notes in a
            // slightly smaller size keep their own scale.
            body: layout::fixed1(block.size) == layout::fixed1(body),
            table: table_like(block, &scan) || independent_list_lines(block),
            prose: prose_block(block),
            figure: label_block(block),
            status,
            source_lines: block
                .lines
                .iter()
                .map(|l| {
                    l.items
                        .iter()
                        .map(|it| scan.origins[it.id].raw.as_str())
                        .collect()
                })
                .collect(),
            maths: parts
                .iter()
                .filter_map(|p| match p {
                    Part::Math { rect, .. } => Some(*rect),
                    Part::Text(_) => None,
                })
                .collect(),
            line_rects: block
                .lines
                .iter()
                .map(|l| Rect {
                    left: l.left,
                    right: l.right,
                    top: l.top,
                    bottom: l.bottom,
                })
                .collect(),
        });
    }
    for p in &mut paragraphs {
        if p.table && p.status == Status::Translate {
            p.status = Status::Retain(Retain::Table);
        }
        if p.align == Align::Center && p.lines == 1 {
            let at_column_left = blocks.iter().any(|b| {
                b.lines.len() >= 2
                    && (b.left - p.rect.left).abs() <= p.size
                    && b.right - b.left >= p.rect.right - p.rect.left - p.size
            });
            if at_column_left || p.rect.right - p.rect.left >= 0.95 * (width - 144.0) {
                p.align = Align::Left;
            }
        }
    }
    let mut found = Found {
        scan,
        blocks,
        others,
        paragraphs,
        chains: Vec::new(),
        kept_underlines: HashSet::new(),
    };
    let mut chains = chains_of(&found.scan);
    for chain in &mut chains {
        chain.along = chain
            .color
            .and_then(|color| classify_decoration(&found, &chain.rect, color));
    }
    found.chains = chains;
    // A line above a baseline may carry deletion semantics. Never erase or
    // silently detach that meaning from its original paragraph.
    for (j, block) in found.blocks.iter().enumerate() {
        if found.scan.decorations.iter().any(|(_, r)| {
            block.lines.iter().any(|l| {
                r.top < l.y
                    && r.top >= l.y - 0.5 * l.size
                    && r.bottom >= l.top
                    && r.left < l.right
                    && r.right > l.left
            })
        }) && found.paragraphs[j].status == Status::Translate
        {
            found.paragraphs[j].status = Status::Retain(Retain::Paint);
        }
    }
    found
}

fn read_doc<'a>(pdfium: &'a Pdfium, bytes: &'a [u8]) -> Result<Doc<'a>, Error> {
    // The same PDFs as the file tab reads (protected ones stop here).
    admit(&pdfium.load_pdf_from_byte_slice(bytes, None)?, bytes)?;
    Doc::load(pdfium.bindings(), bytes)
}

/// The body size of the whole document, whatever pages are worked on: a paragraph is the same
/// with `--pages` as without.
fn document_body(doc: &Doc<'_>) -> Result<(f64, f64), Error> {
    let mut lines = Vec::new();
    for i in 0..doc.pages() {
        let page = doc.page(i)?;
        lines.push(layout::lines_of(scan(doc.lib, &page).items));
    }
    Ok((body_size(&lines), layout::leading_limit(&lines)))
}

fn page_limit(doc: &Doc<'_>, pages: Option<usize>) -> usize {
    pages.map_or(doc.pages(), |n| n.min(doc.pages()))
}

/// The paragraphs of the first `pages` pages (all when `None`).
pub fn analyze(bytes: &[u8], pages: Option<usize>) -> Result<Analysis, Error> {
    let (_guard, pdfium) = pdfium()?;
    let doc = read_doc(&pdfium, bytes)?;
    let count = page_limit(&doc, pages);
    let (body, leading) = document_body(&doc)?;
    let mut analyzed = Vec::new();
    let mut processed_lines = Vec::new();
    for i in 0..count {
        let page = doc.page(i)?;
        let found = verified_found(doc.lib, &page, i, body, leading);
        processed_lines.push(layout::lines_of(found.scan.items.clone()));
        analyzed.push(PageInfo {
            width: found.scan.frame.width,
            height: found.scan.frame.height,
            rotated: found.scan.rotated,
            obstacles: not_in_paragraphs(&found),
            paragraphs: found.paragraphs,
            forms: found.scan.forms,
        });
    }
    Ok(Analysis {
        processed_body_size: body_size(&processed_lines),
        leading_limit: leading,
        pages: doc.pages(),
        body_size: body,
        analyzed,
    })
}

// ---- Setting a translation ----

/// The output font, with its metrics.
pub struct Face<'a> {
    fonts: super::Faces<'a>,
    upem: f64,
}

impl<'a> Face<'a> {
    pub fn parse(bytes: &'a [u8]) -> Result<Self, Error> {
        let face = ttf_parser::Face::parse(bytes, 0).map_err(|_| Error::Font)?;
        Ok(Self::from_fonts(super::Faces {
            ipa_first: true,
            fonts: vec![face],
        }))
    }
    fn from_fonts(fonts: super::Faces<'a>) -> Self {
        Self {
            upem: f64::from(fonts.fonts[0].units_per_em()),
            fonts,
        }
    }
    fn glyph(&self, ch: char) -> Option<(&ttf_parser::Face<'a>, ttf_parser::GlyphId)> {
        self.fonts
            .glyph(ch, false)
            .map(|(i, g)| (&self.fonts.fonts[i], g))
    }
    pub fn has(&self, ch: char) -> bool {
        ch.is_whitespace() || self.glyph(ch).is_some()
    }
    pub fn width(&self, s: &str) -> f64 {
        s.chars()
            .map(|ch| {
                self.glyph(ch)
                    .and_then(|(f, g)| {
                        f.glyph_hor_advance(g)
                            .map(|w| f64::from(w) / f64::from(f.units_per_em()))
                    })
                    .unwrap_or(1.)
            })
            .sum()
    }
}

/// Only list-marker shapes have fallbacks. Unicode identity stays with the CID and
/// ActualText; neither the translation nor the text used for verification is changed.
pub(super) fn translation_glyph(
    face: &ttf_parser::Face<'_>,
    ch: char,
) -> Option<ttf_parser::GlyphId> {
    #[cfg(not(feature = "pdf-erase"))]
    {
        face.glyph_index(ch)
    }
    #[cfg(feature = "pdf-erase")]
    {
        face.glyph_index(ch).or_else(|| {
            let alternative = match ch {
                '∗' => '*',
                '⚫' => '●',
                '➢' => '▶',
                '▪' | '◼' => '■',
                '•' => '・',
                '◦' => '○',
                _ => return None,
            };
            face.glyph_index(alternative)
        })
    }
}

// Not at the start of a line (Japanese kinsoku): closing marks, small kana, the long vowel.
const NO_START: &str = ">＞、。，．,.)）]］｝〕〉》」』】〙〗〟’”｠»ゝゞーァィゥェォッャュョヮヵヶぁぃぅぇぉっゃゅょゎゕゖ々〻‐゠–〜～？！?!‼⁇⁈⁉・：；:;％%";
// Not at the end of a line: opening marks.
const NO_END: &str = "<＜(（[［｛〔〈《「『【〘〖〝‘“｟«";

/// A piece of a line that is not broken: a word, or a CJK character with the marks it holds.
#[derive(Clone, Debug, PartialEq)]
pub struct Atom {
    pub text: String,
    /// Whether white space comes before it.
    pub space: bool,
}

/// The pieces of `text` that a line may break between: English by word, CJK by character,
/// keeping the kinsoku rules (no closing mark at a line's start, no opening one at its end).
pub fn atoms(text: &str) -> Vec<Atom> {
    let mut out: Vec<Atom> = Vec::new();
    let mut space = false;
    for ch in text.chars() {
        if ch.is_whitespace() && ch != '\u{a0}' {
            space = true;
            continue;
        }
        let joins = out.last().is_some_and(|last| {
            let end = last.text.chars().next_back().unwrap_or(' ');
            // A closing mark stays with what is before it, an opening one with what is
            // after it, and a word goes on until a space or a CJK character.
            NO_START.contains(ch)
                || NO_END.contains(end)
                || (!space && !layout::is_cjk(ch) && !layout::is_cjk(end))
        });
        if joins {
            if space {
                out.last_mut().unwrap().text.push(' ');
            }
            out.last_mut().unwrap().text.push(ch);
        } else {
            out.push(Atom {
                text: ch.to_string(),
                space,
            });
        }
        space = false;
    }
    out
}

/// The lines of `atoms` at `size`, none wider than `width`; `None` if a word is wider.
pub fn wrap(atoms: &[Atom], face: &Face<'_>, size: f64, width: f64) -> Option<Vec<(String, f64)>> {
    let space = face.width(" ") * size;
    let mut lines: Vec<(String, f64)> = Vec::new();
    let mut line = String::new();
    let mut w = 0.0;
    for atom in atoms {
        let aw = face.width(&atom.text) * size;
        if aw > width {
            return None;
        }
        let gap = if atom.space && !line.is_empty() {
            space
        } else {
            0.0
        };
        if !line.is_empty() && w + gap + aw > width {
            lines.push((std::mem::take(&mut line), w));
            w = 0.0;
        } else if gap > 0.0 {
            line.push(' ');
            w += gap;
        }
        line.push_str(&atom.text);
        w += aw;
    }
    if !line.is_empty() {
        lines.push((line, w));
    }
    Some(lines)
}

/// A translation set in a paragraph's box: its lines with their baselines, from the page's top
/// left.
#[derive(Clone, Debug)]
struct Setting {
    stage: FitStage,
    line_height: f64,
    scale: f64,
    size: f64,
    lines: Vec<(String, f64, f64)>,
    line_rects: Vec<Rect>,
    wrapped: bool,
    drawn: Rect,
}

#[derive(Clone)]
struct Frame2 {
    left: f64,
    right: f64,
    top: f64,
    /// How far down its lines may go.
    limit: f64,
    narrow_limit: f64,
    size: f64,
    ascent: f64,
    descent: f64,
    line_height: f64,
    align: Align,
    one_line: bool,
    /// Set in one line or not at all (over an underline left as it is drawn).
    single_only: bool,
}

fn set_at(
    atoms: &[Atom],
    face: &Face<'_>,
    f: &Frame2,
    scale: f64,
    single: bool,
) -> Option<Setting> {
    let size = f.size * scale;
    let width = f.right - f.left + 0.5;
    let lines = if single {
        let text = atoms
            .iter()
            .enumerate()
            .map(|(i, a)| {
                if a.space && i > 0 {
                    format!(" {}", a.text)
                } else {
                    a.text.clone()
                }
            })
            .collect::<String>();
        let w = face.width(&text) * size;
        if w > width {
            return None;
        }
        vec![(text, w)]
    } else {
        wrap(atoms, face, size, width)?
    };
    if f.single_only && lines.len() > 1 {
        return None;
    }
    let ascent = f
        .ascent
        .max(f64::from(face.fonts.fonts[0].ascender()) / face.upem);
    let descent = f
        .descent
        .max(-f64::from(face.fonts.fonts[0].descender()) / face.upem);
    // Font metrics and the actual glyph extrema both matter (e.g. accented
    // capitals and Japanese glyphs in a Latin source paragraph).
    let (ascent, descent) = lines
        .iter()
        .flat_map(|(s, _)| s.chars())
        .filter_map(|c| {
            let (f, g) = face.glyph(c)?;
            Some((f.glyph_bounding_box(g)?, f64::from(f.units_per_em())))
        })
        .fold((ascent, descent), |(a, d), (r, em)| {
            (
                a.max(f64::from(r.y_max) / em),
                d.max(-f64::from(r.y_min) / em),
            )
        });
    let lh = f.line_height * size;
    let first = f.top + ascent * size + 0.5;
    let last = first + (lines.len() as f64 - 1.0) * lh;
    if last + descent * size + 0.5 > f.limit + 0.5 {
        return None;
    }
    let set = lines
        .iter()
        .enumerate()
        .map(|(k, (s, w))| {
            let x = match f.align {
                Align::Center => f.left + (f.right - f.left - w) / 2.0,
                Align::Left | Align::Justify => f.left,
            };
            (s.clone(), x, first + k as f64 * lh)
        })
        .collect::<Vec<_>>();
    let line_rects: Vec<_> = set
        .iter()
        .zip(&lines)
        .map(|((text, x, y), (_, w))| {
            let (mut pen, mut min_x, mut max_x) = (0.0_f64, 0.0_f64, *w / size);
            for ch in text.chars() {
                if let Some((font, g)) = face.glyph(ch) {
                    let em = f64::from(font.units_per_em());
                    if let Some(r) = font.glyph_bounding_box(g) {
                        min_x = min_x.min(pen + f64::from(r.x_min) / em);
                        max_x = max_x.max(pen + f64::from(r.x_max) / em);
                    }
                    pen += font.glyph_hor_advance(g).map_or(1.0, |w| f64::from(w) / em);
                }
            }
            Rect {
                // Hinting/rasterization can extend a stem beyond the outline's
                // 0.5 pt padding (observed at 20/24 pt with IPA P Gothic).
                // Reserve this space during fitting AND collision checks.
                left: x + min_x * size - 0.75,
                right: x + max_x * size + 0.75,
                top: y - ascent * size - 0.5,
                bottom: y + descent * size + 0.5,
            }
        })
        .collect();
    Some(Setting {
        stage: FitStage::Original,
        line_height: lh,
        scale,
        size,
        wrapped: f.one_line && set.len() > 1,
        drawn: Rect {
            left: line_rects
                .iter()
                .map(|r| r.left)
                .fold(f64::INFINITY, f64::min),
            right: line_rects
                .iter()
                .map(|r| r.right)
                .fold(f64::NEG_INFINITY, f64::max),
            top: line_rects
                .iter()
                .map(|r| r.top)
                .fold(f64::INFINITY, f64::min),
            bottom: line_rects
                .iter()
                .map(|r| r.bottom)
                .fold(f64::NEG_INFINITY, f64::max),
        },
        line_rects,
        lines: set,
    })
}

fn floor(f: &Frame2, min_scale: f64) -> f64 {
    min_scale.clamp(0.5, 1.0).max(MIN_POINTS / f.size)
}

fn leading_floor(atoms: &[Atom]) -> f64 {
    if atoms.iter().any(|a| a.text.chars().any(layout::is_cjk)) {
        1.3
    } else {
        1.15
    }
}

/// Try each operation in order. The last operation may pass a rectangular obstacle's
/// height only if the actual (possibly narrower) setting is clear of it.
fn fit(
    atoms: &[Atom],
    face: &Face<'_>,
    original: &Frame2,
    expanded: &Frame2,
    high: f64,
    low: f64,
    safe: impl Fn(&Setting) -> bool,
) -> (Option<Setting>, Vec<FitAttempt>) {
    let mut attempts = Vec::new();
    let mut attempt = |f: &Frame2, scale: f64, stage: FitStage, single: bool| {
        let mut used = f.clone();
        let mut setting = set_at(atoms, face, &used, scale, single);
        // A small inset can clear a neighbouring glyph/rule without shrinking
        // the paragraph. Each candidate passes the identical geometric/pixel test.
        if setting.as_ref().is_none_or(|s| !safe(s)) {
            for (dx, dy) in [
                (0.5, 0.5),
                (1.0, 1.0),
                (0.0, -0.5),
                (0.0, -1.0),
                (0.0, -2.0),
                (0.0, -3.0),
                (0.5, -1.0),
                (0.5, -2.0),
                (1.0, -3.0),
            ] {
                if dy < 0.0 && f.top + dy < 36.0_f64.min(original.top) {
                    continue;
                }
                let mut shifted = f.clone();
                if shifted.align != Align::Center {
                    shifted.left += dx;
                }
                shifted.top += dy;
                if let Some(candidate) = set_at(atoms, face, &shifted, scale, single)
                    && safe(&candidate)
                {
                    setting = Some(candidate);
                    used = shifted;
                    break;
                }
            }
        }
        let candidate = setting.as_ref().map(|s| s.drawn);
        let rejection = if let Some(s) = &setting {
            if safe(s) {
                None
            } else {
                setting = None;
                Some("occupied-area-or-page-margin")
            }
        } else {
            Some("box-height-or-unbreakable-word-width")
        };
        if let Some(s) = &mut setting {
            let mut restored = used.clone();
            while s.lines.len() > 1 && restored.line_height < original.line_height - 0.00001 {
                restored.line_height = (restored.line_height + 0.05).min(original.line_height);
                let Some(next) = set_at(atoms, face, &restored, scale, false) else {
                    break;
                };
                if !safe(&next) {
                    break;
                }
                *s = next;
            }
            s.stage = stage;
        }
        attempts.push(FitAttempt {
            stage,
            scale,
            line_height: setting
                .as_ref()
                .map_or(f.line_height * f.size * scale, |s| s.line_height),
            fits: setting.is_some(),
            rejection,
            candidate,
        });
        setting
    };
    // Short source headings and labels get a chance to stay on one line.
    // Long one-line body paragraphs still use normal wrapping.
    if original.one_line && original.right - original.left <= 30.0 * original.size {
        let single_low = low.max(0.85).min(high);
        let steps = ((high - single_low) / 0.05).ceil().max(0.0) as usize;
        for k in 0..=steps {
            let scale = (high - k as f64 * 0.05).max(single_low);
            for (f, stage) in [(original, FitStage::Original), (expanded, FitStage::Expand)] {
                if let Some(s) = attempt(
                    f,
                    scale,
                    if k == 0 { stage } else { FitStage::Shrink },
                    true,
                ) {
                    return (Some(s), attempts);
                }
            }
        }
    }
    if let Some(s) = attempt(original, high, FitStage::Original, false) {
        return (Some(s), attempts);
    }
    let mut f = original.clone();
    let tight = f.line_height.min(leading_floor(atoms));
    while !f.one_line && f.line_height > tight + 0.00001 {
        f.line_height = (f.line_height - 0.1).max(tight);
        if let Some(s) = attempt(&f, high, FitStage::Leading, false) {
            return (Some(s), attempts);
        }
    }
    f.limit = expanded.limit;
    let mut narrow = f.clone();
    narrow.limit = expanded.narrow_limit;
    f.left = expanded.left;
    f.right = expanded.right;
    let mut variants = vec![f];
    if narrow.right != expanded.right {
        variants.push(narrow);
    }
    for f in &variants {
        if let Some(s) = attempt(f, high, FitStage::Expand, false) {
            return (Some(s), attempts);
        }
    }
    // An expanded width can be unusable (for example on a shaded slide). Keep
    // trying the original width at the SAME scale before making any text smaller.
    let steps = ((high - low) / 0.05).ceil().max(0.0) as usize;
    for k in 1..=steps {
        let scale = (high - k as f64 * 0.05).max(low);
        for f in &variants {
            if let Some(s) = attempt(f, scale, FitStage::Shrink, false) {
                return (Some(s), attempts);
            }
        }
    }
    // Only the height constraint is removed. The safety predicate still enforces
    // the page margin, source obstacles and the erased page's unoccupied pixels.
    for f in &mut variants {
        f.limit = f64::INFINITY;
        if let Some(s) = attempt(f, low, FitStage::Overflow, false) {
            return (Some(s), attempts);
        }
    }
    (None, attempts)
}

/// A new portion of a rectangle must be empty. Existing intersections are not enlarged.
fn clear_expansion(source: &Rect, drawn: &Rect, obstacles: &[Rect]) -> bool {
    obstacles
        .iter()
        .all(|o| match (meet(drawn, o), meet(source, o)) {
            (None, _) => true,
            (Some(_), None) => false,
            (Some((w, h)), Some((w0, h0))) => w <= w0 && h <= h0,
        })
}

const AUDIT_SCALE: f64 = 2.0;
type Bitmap = (usize, usize, Vec<u8>);

fn pixels_in<'a>(r: &Rect, bitmap: &'a Bitmap) -> impl Iterator<Item = [u8; 3]> + 'a {
    let (w, h, pixels) = bitmap;
    let (x0, y0) = (
        (r.left * AUDIT_SCALE - 0.5).ceil().max(0.0) as usize,
        (r.top * AUDIT_SCALE - 0.5).ceil().max(0.0) as usize,
    );
    let (x1, y1) = (
        ((r.right * AUDIT_SCALE + 0.5).floor().max(0.0) as usize).min(*w),
        ((r.bottom * AUDIT_SCALE + 0.5).floor().max(0.0) as usize).min(*h),
    );
    (y0..y1).flat_map(move |y| {
        (x0..x1).map(move |x| {
            let k = (y * w + x) * 4;
            [pixels[k], pixels[k + 1], pixels[k + 2]]
        })
    })
}

/// The modal surrounding background, quantized only for choosing the sample.
/// Content must then be within 12 levels of that sample in every channel. This
/// accepts uniform colored paper and near-white antialias noise, not dark marks.
fn background(r: &Rect, bitmap: &Bitmap) -> [u8; 3] {
    let area = Rect {
        left: r.left - 3.0,
        right: r.right + 3.0,
        top: r.top - 3.0,
        bottom: r.bottom + 3.0,
    };
    let mut colors: HashMap<[u8; 3], (usize, [u8; 3])> = HashMap::new();
    for rgb in pixels_in(&area, bitmap) {
        let entry = colors.entry(rgb.map(|v| v / 16)).or_insert((0, rgb));
        entry.0 += 1;
    }
    colors
        .values()
        .max_by_key(|(n, _)| *n)
        .map_or([255; 3], |(_, c)| *c)
}

fn clear_pixels(r: &Rect, color: [u8; 3], bitmap: &Bitmap) -> bool {
    // At 144 dpi the independent raster audit treats a one-pixel neighbour of
    // a content edge as touching. Reserve that clearance around the glyph box.
    let guarded = Rect {
        left: r.left - 0.5,
        right: r.right + 0.5,
        top: r.top - 0.5,
        bottom: r.bottom + 0.5,
    };
    bitmap.0 > 0
        && bitmap.1 > 0
        && pixels_in(&guarded, bitmap).all(|rgb| (0..3).all(|c| rgb[c].abs_diff(color[c]) <= 12))
}

fn clear_lines(s: &Setting, color: [u8; 3], bitmap: &Bitmap) -> bool {
    s.line_rects.iter().all(|r| clear_pixels(r, color, bitmap))
}

/// Endpoints must coincide with a continuous run of real text, in its color.
/// A synthetic whitespace item joining separate table cells is not text.
fn classify_decoration(found: &Found, r: &Rect, color: [u32; 4]) -> Option<usize> {
    let mut owners = Vec::new();
    for (j, block) in found.blocks.iter().enumerate() {
        for line in &block.lines {
            if r.right - r.left <= 2.0
                || r.bottom - r.top > 1.0
                || r.top < line.y
                || r.bottom > line.y + 0.45 * line.size
            {
                continue;
            }
            let mut spans: Vec<_> = line
                .items
                .iter()
                .filter(|it| {
                    found.scan.origins[it.id].object != 0
                        && !it.str.trim().is_empty()
                        && it.x >= r.left - 1.0
                        && it.x + it.w <= r.right + 1.0
                })
                .collect();
            spans.sort_by(|a, b| a.x.total_cmp(&b.x));
            if let (Some(first), Some(last)) = (spans.first(), spans.last())
                && (first.x - r.left).abs() <= 1.0
                && (last.x + last.w - r.right).abs() <= 1.0
                && spans
                    .iter()
                    .all(|it| found.scan.origins[it.id].color == color)
                && spans
                    .windows(2)
                    .all(|pair| pair[1].x - pair[0].x - pair[0].w <= 0.6 * line.size)
            {
                owners.push(j);
            }
        }
    }
    owners.sort_unstable();
    owners.dedup();
    (owners.len() == 1).then(|| owners[0])
}

/// The page's thin rules joined into chains: pieces of one color and height (within 0.3 pt)
/// that touch or overlap end to end (within 0.5 pt).
fn chains_of(scan: &Scan) -> Vec<Chain> {
    let n = scan.decorations.len();
    let mut group: Vec<usize> = (0..n).collect();
    fn root(group: &mut [usize], mut k: usize) -> usize {
        while group[k] != k {
            group[k] = group[group[k]];
            k = group[k];
        }
        k
    }
    let color = |k: usize| scan.decoration_colors.get(&scan.decorations[k].0).copied();
    for a in 0..n {
        for b in a + 1..n {
            let (ra, rb) = (&scan.decorations[a].1, &scan.decorations[b].1);
            if color(a).is_some()
                && color(a) == color(b)
                && (ra.top - rb.top).abs() <= 0.3
                && (ra.bottom - rb.bottom).abs() <= 0.3
                && ra.left <= rb.right + 0.5
                && rb.left <= ra.right + 0.5
            {
                let (x, y) = (root(&mut group, a), root(&mut group, b));
                group[x] = y;
            }
        }
    }
    let mut chains: Vec<Chain> = Vec::new();
    let mut at: HashMap<usize, usize> = HashMap::new();
    for k in 0..n {
        let r = scan.decorations[k].1;
        let g = root(&mut group, k);
        match at.get(&g) {
            Some(&c) => {
                let chain = &mut chains[c];
                chain.pieces.push(k);
                chain.rect = Rect {
                    left: chain.rect.left.min(r.left),
                    right: chain.rect.right.max(r.right),
                    top: chain.rect.top.min(r.top),
                    bottom: chain.rect.bottom.max(r.bottom),
                };
            }
            None => {
                at.insert(g, chains.len());
                chains.push(Chain {
                    pieces: vec![k],
                    rect: r,
                    color: color(k),
                    along: None,
                });
            }
        }
    }
    chains
}

fn verified_found(
    lib: &dyn PdfiumLibraryBindings,
    page: &Page<'_>,
    index: usize,
    body: f64,
    leading: f64,
) -> Found {
    kept_with_rules(find_with_leading(scan(lib, page), index, body, leading))
}

/// No rule is taken off a page, and no old underline is to be left beneath new words: a
/// paragraph with a thin rule in the text band of one of its lines stays as it is drawn, rule
/// and all (`Retain::Decoration`); but one of one line underlined end to end, which may be set
/// over its rule ([`kept_underline`]). Whatever is over the rule (a link annotation) or however
/// it is drawn, it is no paragraph's.
fn kept_with_rules(mut found: Found) -> Found {
    for (j, block) in found.blocks.iter().enumerate() {
        if found.paragraphs[j].status == Status::Translate
            && found.scan.decorations.iter().any(|(_, r)| {
                block.lines.iter().any(|line| {
                    r.right > line.left
                        && r.left < line.right
                        && r.top >= line.top
                        && r.top <= line.y + 0.45 * line.size
                })
            })
        {
            if kept_underline(&found, j) {
                found.kept_underlines.insert(j);
            } else {
                found.paragraphs[j].status = Status::Retain(Retain::Decoration);
            }
        }
    }
    found
}

/// Whether paragraph `j`, of one line, has along it one rule from end to end of the line, and
/// no other (a heading's underline, a link's, a table's rule: left as it is drawn, as every rule
/// is): its translation may be set over it, the rule staying under it, where the translation,
/// in one line, is clear of it by every check of a placement (the rule is drawn on the page they
/// look at). A rule under some words of the line is not: left under other words of the
/// translation, it would tell what is not so.
fn kept_underline(found: &Found, j: usize) -> bool {
    let [line] = &found.blocks[j].lines[..] else {
        return false;
    };
    let along = |r: &Rect| {
        r.right > line.left
            && r.left < line.right
            && r.top >= line.top
            && r.top <= line.y + 0.45 * line.size
    };
    let whole: Vec<&Chain> = found
        .chains
        .iter()
        .filter(|c| {
            c.along == Some(j)
                && (c.rect.left - line.left).abs() <= 1.0
                && (c.rect.right - line.right).abs() <= 1.0
        })
        .collect();
    let [chain] = whole[..] else {
        return false;
    };
    found
        .scan
        .decorations
        .iter()
        .enumerate()
        .filter(|(_, (_, r))| along(r))
        .all(|(k, _)| chain.pieces.contains(&k))
}

/// The text objects of paragraph `j`: what goes off the page with it (its text, and nothing
/// drawn: no path of a page is ever taken off).
fn paragraph_objects(found: &Found, j: usize) -> Vec<usize> {
    found.blocks[j]
        .lines
        .iter()
        .flat_map(|l| &l.items)
        .map(|it| found.scan.origins[it.id].object)
        .filter(|o| *o != 0)
        .collect()
}

/// A thin rule along a line, for the developer command: where it is (page space `[left, bottom,
/// right, top]`), its pieces, the paragraph whose line it runs along end to end, and whether
/// that paragraph's translation may be set over it ([`kept_underline`]). Every rule is left as
/// it is drawn.
#[derive(Clone, Debug)]
pub struct RuleReport {
    pub page: usize,
    pub rect: [f64; 4],
    pub pieces: Vec<[f64; 4]>,
    pub paragraph: Option<String>,
    pub kept: bool,
}

/// The thin rules of the first `pages` pages, as the save takes them ([`verified_found`]).
pub fn rule_report(bytes: &[u8], pages: Option<usize>) -> Result<Vec<RuleReport>, Error> {
    let (_guard, pdfium) = pdfium()?;
    let doc = read_doc(&pdfium, bytes)?;
    let (body, leading) = document_body(&doc)?;
    let mut out = Vec::new();
    for i in 0..page_limit(&doc, pages) {
        let found = verified_found(doc.lib, &doc.page(i)?, i, body, leading);
        let corners = |r: &Rect| {
            let (l, b, w, h) = found.scan.frame.page_rect(r);
            [l, b, l + w, b + h]
        };
        for c in &found.chains {
            out.push(RuleReport {
                page: i,
                rect: corners(&c.rect),
                pieces: c
                    .pieces
                    .iter()
                    .map(|&p| corners(&found.scan.decorations[p].1))
                    .collect(),
                paragraph: c.along.map(|j| found.paragraphs[j].id.clone()),
                kept: c.along.is_some_and(|j| found.kept_underlines.contains(&j)),
            });
        }
    }
    Ok(out)
}

/// The boxes, in pixels at `scale`, outside which a write may not lose any ink on page
/// `page`: [`translated_boxes`], and the masks of the glyphs erased with them (PDFium's boxes
/// of their ink, which a descender can take out of a paragraph's box).
pub fn kept_boxes(
    original: &[u8],
    outcomes: &[&Outcome],
    page: usize,
    scale: f64,
) -> Result<Vec<Rect>, Error> {
    let (_guard, pdfium) = pdfium()?;
    let doc = read_doc(&pdfium, original)?;
    let (body, leading) = document_body(&doc)?;
    let pdf_page = doc.page(page)?;
    let found = verified_found(doc.lib, &pdf_page, page, body, leading);
    let mut boxes = translated_boxes(outcomes, scale);
    for (j, p) in found.paragraphs.iter().enumerate() {
        if outcomes
            .iter()
            .any(|o| o.paragraph.id == p.id && o.status == Status::Translate)
        {
            let glyphs = selected_characters(doc.lib, &pdf_page, &found, j, false)?;
            boxes.extend(erase_boxes(&glyphs, page, scale));
        }
    }
    Ok(boxes)
}

/// Remove eligible source objects on a private copy. Kept objects can be restored
/// after fitting; their pixels then participate in the final safety pass.
fn take_text(
    lib: &dyn PdfiumLibraryBindings,
    page: &Page<'_>,
    found: &Found,
    selected: impl Fn(usize) -> bool,
) -> Result<Vec<(usize, FPDF_PAGEOBJECT)>, Error> {
    let mut taken = Vec::new();
    let mut seen = HashSet::new();
    for (j, _block) in found
        .blocks
        .iter()
        .enumerate()
        .filter(|(j, _)| selected(*j))
    {
        for object in paragraph_objects(found, j) {
            if object != 0 && seen.insert(object) {
                if lib.FPDFPage_RemoveObject(page.raw, object as FPDF_PAGEOBJECT) == 0 {
                    for (_, obj) in taken {
                        lib.FPDFPage_InsertObject(page.raw, obj);
                    }
                    return Err(internal());
                }
                taken.push((j, object as FPDF_PAGEOBJECT));
            }
        }
    }
    Ok(taken)
}

/// Estimate the column edge from paragraphs with a common left edge. The upper
/// decile resists a single spanning title; a short final line does not define a gutter.
fn column_right(source: &Rect, size: f64, page_width: f64, paragraphs: &[Rect]) -> f64 {
    let mut edges: Vec<f64> = paragraphs
        .iter()
        .filter(|r| (r.left - source.left).abs() <= 2.0 * size && r.right >= source.right - size)
        .map(|r| r.right)
        .collect();
    edges.sort_by(f64::total_cmp);
    let edge = if edges.len() >= 3 {
        edges[((edges.len() - 1) as f64 * 0.9).floor() as usize]
    } else {
        source.right
    };
    let neighbor = paragraphs
        .iter()
        .filter(|r| {
            r.left >= source.right
                && r.bottom > source.top
                && r.top < source.bottom
                && r.left > source.left + 8.0 * size
                && r.right - r.left >= 8.0 * size
        })
        .map(|r| r.left)
        .min_by(f64::total_cmp);
    neighbor
        .map_or(page_width - 36.0, |x| {
            if source.right - source.left <= 12.0 * size {
                x - size
            } else {
                edge.max(source.right).min(x - size)
            }
        })
        .min(page_width - 36.0)
        .max(source.right)
}

fn expanded_frame(found: &Found, index: usize, original: &Frame2) -> Frame2 {
    let mut f = original.clone();
    let p = &found.paragraphs[index];
    let obstacles = obstacles_of(found, index);
    if f.one_line {
        f.right = column_right(
            &p.rect,
            p.size,
            found.scan.frame.width,
            &found.paragraphs.iter().map(|p| p.rect).collect::<Vec<_>>(),
        );
        // Only content at this height limits horizontal expansion. In particular,
        // a centered page number at the foot is not a neighboring column.
        for o in &obstacles {
            if o.top < p.rect.bottom - 1.0 && o.bottom > p.rect.top + 1.0 && o.right > p.rect.right
            {
                f.right = f.right.min(if o.left >= p.rect.right {
                    o.left - 1.0
                } else {
                    p.rect.right
                });
            }
        }
        f.right = f.right.max(p.rect.right);
        if f.align == Align::Center {
            let center = (p.rect.left + p.rect.right) / 2.0;
            let left_limit = obstacles
                .iter()
                .filter(|o| o.top < p.rect.bottom && o.bottom > p.rect.top && o.left < p.rect.left)
                .map(|o| (o.right + 1.0).min(p.rect.left))
                .fold(36.0_f64.min(p.rect.left), f64::max);
            let half = (f.right - center)
                .min(center - left_limit)
                .max((p.rect.right - p.rect.left) / 2.0);
            f.left = center - half;
            f.right = center + half;
        }
    }
    let mut limit = (found.scan.frame.height - 36.0).max(p.rect.bottom);
    for o in &obstacles {
        if o.left < f.right && o.right > f.left && o.bottom > p.rect.bottom {
            limit = limit.min(if o.top >= p.rect.bottom {
                o.top - 1.0
            } else {
                p.rect.bottom
            });
        }
    }
    f.limit = limit.max(p.rect.bottom);
    f.narrow_limit = limit_below(found, index);
    f
}

fn majority_style(found: &Found, j: usize) -> (String, bool, [u32; 4]) {
    type Style = (String, bool, [u32; 4]);
    let mut votes: Vec<(Style, usize)> = Vec::new();
    for it in found.blocks[j].lines.iter().flat_map(|l| &l.items) {
        let origin = &found.scan.origins[it.id];
        let key = (layout::fixed1(it.size), it.bold, origin.color);
        if let Some((_, n)) = votes.iter_mut().find(|(k, _)| *k == key) {
            *n += origin.count;
        } else {
            votes.push((key, origin.count));
        }
    }
    votes.into_iter().max_by_key(|(_, n)| *n).unwrap().0
}

fn uniform_anchor(found: &Found, j: usize) -> bool {
    let p = &found.paragraphs[j];
    let column_start = p.lines <= 3
        && !found.paragraphs.iter().enumerate().any(|(k, q)| {
            k != j
                && q.rect.bottom <= p.rect.top
                && q.rect.right > p.rect.left
                && q.rect.left < p.rect.right
                && layout::fixed1(q.size) == layout::fixed1(p.size)
        });
    let vertical_side = |left: bool| {
        found.scan.drawings.iter().any(|r| {
            r.right - r.left <= 1.0
                && r.bottom >= p.rect.bottom
                && r.top <= p.rect.top
                && if left {
                    r.right <= p.rect.left && p.rect.left - r.right < 2.0 * p.size
                } else {
                    r.left >= p.rect.right && r.left - p.rect.right < 2.0 * p.size
                }
        })
    };
    !column_start && !(vertical_side(true) && vertical_side(false))
}

fn bullet_item(text: &str) -> bool {
    text.trim_start()
        .starts_with(['•', '●', '⚫', '◼', '▪', '➢', '◆', '✓', '○', '・'])
}

fn hanging_continuation(a: &Rect, b: &Rect, size: f64) -> bool {
    let indent = b.left - a.left;
    (0.8 * size..=2.0 * size).contains(&indent)
        && b.top >= a.bottom - 0.5
        && b.top <= a.bottom + 0.6 * size
        && (a.right - b.right).abs() <= 3.0 * size
        && a.right - a.left >= 10.0 * size
}

fn frame_of(block: &Block, p: &Paragraph, limit: f64) -> Frame2 {
    let first = &block.lines[0];
    let last = block.lines.last().unwrap();
    let size = block.size;
    Frame2 {
        left: block.left,
        right: block.right,
        top: block.top,
        limit,
        narrow_limit: limit,
        size,
        ascent: ((first.y - block.top) / size).clamp(0.6, 1.1),
        descent: ((block.bottom - last.y) / size).clamp(0.1, 0.4),
        line_height: p.line_height,
        align: p.align,
        one_line: block.lines.len() == 1,
        single_only: false,
    }
}

/// How far down a paragraph may go: to the next text or drawing below it that it would run
/// into, or near the page's foot. Whatever is beside it (across its width) and goes on below its
/// foot is in the way: from where it starts, or, if it starts above the foot, at once (the
/// paragraph is not extended). A box around the paragraph is its sides ([`Scan::drawings`]): the
/// paragraph goes down to the one below it, and stays inside (as it does inside a table's cell);
/// an image or a shading under it keeps it to its own box.
fn limit_below(found: &Found, index: usize) -> f64 {
    let block = &found.blocks[index];
    let r = Rect {
        left: block.left,
        right: block.right,
        top: block.top,
        bottom: block.bottom,
    };
    let mut limit = (found.scan.frame.height - 36.0).max(r.bottom);
    let mut consider = |o: &Rect| {
        if !(o.left < r.right && o.right > r.left && o.bottom > r.bottom + 0.5) {
            return;
        }
        if o.top >= r.bottom - 0.5 {
            limit = limit.min(o.top - 1.0);
        } else {
            limit = r.bottom;
        }
    };
    for o in obstacles_of(found, index) {
        consider(&o);
    }
    limit.max(r.bottom)
}

/// What a paragraph's translation may not be drawn over, but the other paragraphs (whose boxes
/// [`overlaps`] sees): the lines of the other paragraphs, the lines in none, text in no piece
/// (rotated, mirrored, tiny, on the page or in a Form), and drawings.
fn obstacles_of(found: &Found, index: usize) -> Vec<Rect> {
    let mut out = Vec::new();
    for (j, b) in found.blocks.iter().enumerate() {
        if j == index
            || matches!(
                found.paragraphs[j].status,
                Status::Skip(Skip::Invisible | Skip::ImageText)
            )
        {
            continue;
        }
        out.extend(b.lines.iter().map(|l| Rect {
            left: l.left,
            right: l.right,
            top: l.top,
            bottom: l.bottom,
        }));
    }
    out.extend(not_in_paragraphs(found).into_iter().filter(|r| {
        !found
            .scan
            .links
            .iter()
            .any(|link| link == r && own_link(found, index, link))
    }));
    out
}

fn own_link(found: &Found, index: usize, link: &Rect) -> bool {
    let p = &found.paragraphs[index].rect;
    meet(p, link).is_some()
        && !found
            .paragraphs
            .iter()
            .enumerate()
            .any(|(j, other)| j != index && meet(&other.rect, link).is_some())
}

/// What [`obstacles_of`] has of no paragraph.
fn not_in_paragraphs(found: &Found) -> Vec<Rect> {
    let mut out = found.others.clone();
    // Text in no piece: rotated, mirrored, or tiny.
    let pieces: HashSet<usize> = found.scan.origins.iter().map(|o| o.object).collect();
    out.extend(
        found
            .scan
            .text_bounds
            .iter()
            .filter(|(object, _)| !pieces.contains(object))
            .map(|(_, o)| *o),
    );
    out.extend(found.scan.drawings.iter().copied());
    out.extend(found.scan.links.iter().copied());
    out.retain(|r| (r.right - r.left).max(0.1) * (r.bottom - r.top).max(0.1) >= 1.0);
    out
}

/// Translations, by paragraph id, written over the first `pages` pages (all when `None`).
pub fn write(
    bytes: &[u8],
    pages: Option<usize>,
    translations: &HashMap<String, String>,
    options: Options,
) -> Result<(Vec<u8>, Vec<Outcome>), Error> {
    let (out, outcomes, _) = run(
        bytes,
        pages,
        translations,
        options,
        WriteMode::Pdfium,
        false,
        None,
    )?;
    Ok((out, outcomes))
}

/// A selected source character. Origin is PDF user space, rect is displayed top-left points.
#[derive(Clone, Debug)]
pub struct EraseChar {
    pub paragraph: String,
    pub page: usize,
    pub index: i32,
    pub unicode: u32,
    /// PDFium IsHyphen provenance, not inferred from the Unicode value.
    pub is_hyphen: bool,
    pub origin: (f64, f64),
    pub rect: Rect,
}

/// Select only paragraphs whose translations fit, without modifying the source.
pub fn erase_plan(
    bytes: &[u8],
    pages: Option<usize>,
    translations: &HashMap<String, String>,
    options: Options,
) -> Result<(Vec<Outcome>, Vec<EraseChar>), Error> {
    let (_, outcomes, chars) = run(
        bytes,
        pages,
        translations,
        options,
        WriteMode::Plan,
        false,
        None,
    )?;
    Ok((outcomes, chars))
}

/// Replan only failed pages; document-wide paragraph/style analysis is unchanged.
pub fn erase_plan_pages(
    bytes: &[u8],
    selected: &HashSet<usize>,
    translations: &HashMap<String, String>,
    options: Options,
) -> Result<(Vec<Outcome>, Vec<EraseChar>), Error> {
    let (_, outcomes, chars) = run(
        bytes,
        None,
        translations,
        options,
        WriteMode::Plan,
        false,
        Some(selected),
    )?;
    Ok((outcomes, chars))
}

/// The comparison eraser, without inserting translations.
pub fn erase_pdfium(
    bytes: &[u8],
    pages: Option<usize>,
    translations: &HashMap<String, String>,
    options: Options,
) -> Result<(Vec<u8>, Vec<Outcome>), Error> {
    let (out, outcomes, _) = run(
        bytes,
        pages,
        translations,
        options,
        WriteMode::Pdfium,
        true,
        None,
    )?;
    Ok((out, outcomes))
}

/// Layout is computed from the original. The caller must erase precisely `erase_plan`.
pub fn write_erased(
    bytes: &[u8],
    erased: &[u8],
    pages: Option<usize>,
    translations: &HashMap<String, String>,
    options: Options,
) -> Result<(Vec<u8>, Vec<Outcome>), Error> {
    let (out, outcomes, _) = run(
        bytes,
        pages,
        translations,
        options,
        WriteMode::Erased(erased),
        false,
        None,
    )?;
    Ok((out, outcomes))
}

/// A link that goes with its paragraph's translation: the page (from 0), the link's
/// rectangle and the translation's box, in page space (`[left, bottom, right, top]`).
#[derive(Clone, Debug, PartialEq)]
pub struct FollowedLink {
    pub page: usize,
    pub link: [f64; 4],
    pub drawn: [f64; 4],
}

/// The links of the paragraphs translated that go to their translation's box: the one link
/// that meets a paragraph's box (and no other paragraph's), of a paragraph of one line or
/// mostly covered by it (80% of its box). Which words of a translation a link was on cannot
/// be told, so it takes the whole translation; that is not done to a link on a few words of
/// a longer paragraph (a citation would take a whole paragraph), nor to a paragraph with two
/// links or more: those stay where they are. Of these, [`super::erase::move_links`] moves
/// only links to a URL.
pub fn followed_links(
    original: &[u8],
    outcomes: &[Outcome],
    pages: Option<usize>,
) -> Result<Vec<FollowedLink>, Error> {
    let translated: Vec<&Outcome> = outcomes
        .iter()
        .filter(|o| o.status == Status::Translate && o.drawn.is_some())
        .collect();
    let mut on: Vec<usize> = translated.iter().map(|o| o.paragraph.page).collect();
    on.sort_unstable();
    on.dedup();
    let mut out = Vec::new();
    if on.is_empty() {
        return Ok(out);
    }
    let (_guard, pdfium) = pdfium()?;
    let doc = read_doc(&pdfium, original)?;
    let (body, leading) = document_body(&doc)?;
    for i in on.into_iter().filter(|&i| i < page_limit(&doc, pages)) {
        let page = doc.page(i)?;
        if doc.lib.FPDFPage_GetAnnotCount(page.raw) <= 0 {
            continue;
        }
        let found = verified_found(doc.lib, &page, i, body, leading);
        for o in translated.iter().filter(|o| o.paragraph.page == i) {
            let (Some(j), Some(drawn)) = (
                found.paragraphs.iter().position(|p| p.id == o.paragraph.id),
                o.drawn,
            ) else {
                continue;
            };
            let rect = found.paragraphs[j].rect;
            let meeting: Vec<&Rect> = found
                .scan
                .links
                .iter()
                .filter(|l| meet(&rect, l).is_some())
                .collect();
            let area = |r: &Rect| (r.right - r.left).max(0.0) * (r.bottom - r.top).max(0.0);
            if let [link] = meeting[..]
                && own_link(&found, j, link)
                && (found.blocks[j].lines.len() == 1
                    || meet(&rect, link).is_some_and(|(w, h)| w * h >= 0.8 * area(&rect)))
            {
                let corners = |r: &Rect| {
                    let (l, b, w, h) = found.scan.frame.page_rect(r);
                    [l, b, l + w, b + h]
                };
                out.push(FollowedLink {
                    page: i,
                    link: corners(link),
                    drawn: corners(&drawn),
                });
            }
        }
    }
    Ok(out)
}

/// Product-facing insertion boundary: a page is restored from the original if
/// its erased paragraph set differs from the final inserted paragraph set.
pub fn write_checked_erased(
    original: &[u8],
    erased: &[u8],
    pages: Option<usize>,
    translations: &HashMap<String, String>,
    options: Options,
    erased_ids: &[String],
) -> Result<(Vec<u8>, Vec<Outcome>), Error> {
    let (written, mut outcomes) = write_erased(original, erased, pages, translations, options)?;
    let mut restore = HashSet::new();
    for o in &outcomes {
        if erased_ids.contains(&o.paragraph.id) != (o.status == Status::Translate) {
            restore.insert(o.paragraph.page);
        }
    }
    // Unknown IDs cannot be mapped safely to a page.
    if erased_ids
        .iter()
        .any(|id| !outcomes.iter().any(|o| &o.paragraph.id == id))
    {
        restore.extend(outcomes.iter().map(|o| o.paragraph.page));
    }
    if restore.is_empty() {
        return Ok((written, outcomes));
    }
    let (_guard, pdfium) = pdfium()?;
    let source = read_doc(&pdfium, original)?;
    let dest = read_doc(&pdfium, &written)?;
    let mut restored: Vec<_> = restore.iter().copied().collect();
    restored.sort_unstable();
    for page in restored {
        dest.lib.FPDFPage_Delete(dest.raw, page as i32);
        if dest.lib.FPDF_ImportPagesByIndex_vec(
            dest.raw,
            source.raw,
            vec![page as i32],
            page as i32,
        ) == 0
        {
            return Err(internal());
        }
    }
    for o in &mut outcomes {
        if restore.contains(&o.paragraph.page)
            && (o.status == Status::Translate || erased_ids.contains(&o.paragraph.id))
        {
            o.status = Status::Retain(Retain::PlanMismatch);
            o.translation = None;
            o.drawn = None;
            o.line_rects.clear();
            o.scale = None;
            o.fit_stage = None;
            o.line_height = None;
            o.drawn_lines = 0;
            o.wrapped = false;
        }
    }
    Ok((dest.save()?, outcomes))
}

#[derive(Clone, Copy)]
enum WriteMode<'a> {
    Pdfium,
    Plan,
    Erased(&'a [u8]),
}

type RunResult = (Vec<u8>, Vec<Outcome>, Vec<EraseChar>);

/// Layout-only font. The default bilingual writer keeps output_font().
fn inplace_font() -> Result<Vec<u8>, Error> {
    match std::env::var_os("PRIVATE_TRANSLATE_PDF_INPLACE_FONT") {
        Some(path) => {
            let bytes = std::fs::read(path).map_err(|_| Error::Font)?;
            Face::parse(&bytes)?;
            Ok(bytes)
        }
        None => {
            let exe = std::env::current_exe().unwrap_or_default();
            let mut candidates = Vec::new();
            if let Some(path) = std::env::var_os("PRIVATE_TRANSLATE_PDF_FONT")
                && let Some(parent) = std::path::Path::new(&path).parent()
            {
                candidates.push(parent.join("ipagp.ttf"));
            }
            for dir in super::resource_dirs(&exe, cfg!(target_os = "macos")) {
                candidates.push(dir.join("ipafont/ipagp.ttf"));
                candidates.push(dir.join("ipagp.ttf"));
            }
            if cfg!(target_os = "macos")
                && let Some(home) = std::env::var_os("HOME")
            {
                candidates.push(std::path::PathBuf::from(home).join("Library/Fonts/ipagp.ttf"));
            }
            for path in candidates {
                if let Ok(bytes) = std::fs::read(path)
                    && Face::parse(&bytes).is_ok_and(|f| f.has('日') && f.has('A'))
                {
                    return Ok(bytes);
                }
            }
            output_font()
        }
    }
}

fn run(
    bytes: &[u8],
    pages: Option<usize>,
    translations: &HashMap<String, String>,
    options: Options,
    mode: WriteMode<'_>,
    erase_only: bool,
    selected_pages: Option<&HashSet<usize>>,
) -> Result<RunResult, Error> {
    let font_bytes = inplace_font()?;
    #[cfg(feature = "pdf-erase")]
    let noto = super::Noto::load();
    #[cfg(feature = "pdf-erase")]
    let face = Face::from_fonts(super::Faces::new(&font_bytes, &noto, options.family)?);
    #[cfg(not(feature = "pdf-erase"))]
    let face = Face::parse(&font_bytes)?;
    let (_guard, pdfium) = pdfium()?;
    let doc = read_doc(&pdfium, bytes)?;
    let graphics = read_doc(&pdfium, bytes)?;
    let lib = doc.lib;
    let external = match mode {
        WriteMode::Erased(bytes) => Some(read_doc(&pdfium, bytes)?),
        _ => None,
    };
    let destination = external.as_ref().unwrap_or(&doc);
    if destination.pages() != doc.pages() {
        return Err(internal());
    }
    let count = page_limit(&doc, pages);
    let (body, leading) = document_body(&doc)?;
    #[cfg(not(feature = "pdf-erase"))]
    let font = Font {
        lib,
        raw: lib.FPDFText_LoadFont(
            destination.raw,
            font_bytes.as_ptr(),
            font_bytes.len() as u32,
            FONT_TRUETYPE,
            1,
        ),
    };
    #[cfg(not(feature = "pdf-erase"))]
    if font.raw.is_null() {
        return Err(Error::Font);
    }
    #[cfg(feature = "pdf-erase")]
    let mut translation_lines = Vec::new();
    let mut outcomes = Vec::new();
    let mut characters = Vec::new();
    for i in 0..count {
        if selected_pages.is_some_and(|pages| !pages.contains(&i)) {
            continue;
        }
        let page = doc.page(i)?;
        let found = verified_found(lib, &page, i, body, leading);
        let graphics_page = graphics.page(i)?;
        let graphics_found = verified_found(lib, &graphics_page, i, body, leading);
        let mut taken = take_text(lib, &graphics_page, &graphics_found, |j| {
            let p = &found.paragraphs[j];
            p.status == Status::Translate
                && translations
                    .get(&p.id)
                    .is_some_and(|t| !t.trim().is_empty() && t.chars().all(|c| face.has(c)))
        })?;
        let mut bitmap = render(lib, &graphics_page, AUDIT_SCALE);
        // Broad filled/image bounds can include both blank background and distant
        // artwork. Their actual pixels remain mandatory obstacles for every line;
        // the enclosing rectangle must not block all expansion on a slide.
        let page_area = found.scan.frame.width * found.scan.frame.height;
        let mut found = found;
        found
            .scan
            .drawings
            .retain(|r| (r.right - r.left) * (r.bottom - r.top) <= page_area * 0.2);
        let target_page = external.as_ref().map(|d| d.page(i)).transpose()?;
        let target = target_page.as_ref().unwrap_or(&page);
        let target_frame = Frame::of(target);
        if target_frame.width != found.scan.frame.width
            || target_frame.height != found.scan.frame.height
            || target_frame.turns != found.scan.frame.turns
        {
            return Err(internal());
        }
        // Fit each paragraph, then harmonize same-format paragraphs on this page.
        let mut settings: Vec<Option<Setting>> = Vec::new();
        let mut statuses = Vec::new();
        let mut frames = Vec::new();
        let mut expanded_frames = Vec::new();
        let mut fit_attempts = Vec::new();
        for (j, p) in found.paragraphs.iter().enumerate() {
            let mut f = frame_of(&found.blocks[j], p, p.rect.bottom);
            let text = translations.get(&p.id).map(|t| t.trim()).unwrap_or("");
            let unavailable = if p.status != Status::Translate {
                Some(p.status)
            } else if found.blocks[j]
                .lines
                .iter()
                .flat_map(|l| &l.items)
                .any(|it| found.scan.origins[it.id].unsafe_paint)
            {
                Some(Status::Retain(Retain::Paint))
            } else if text.is_empty() {
                Some(Status::Retain(Retain::Missing))
            } else if !text.chars().all(|c| face.has(c)) {
                Some(Status::Retain(Retain::Glyph))
            } else {
                None
            };
            if let Some(status) = unavailable {
                statuses.push(status);
                settings.push(None);
                expanded_frames.push(f.clone());
                frames.push(f);
                fit_attempts.push(Vec::new());
                continue;
            }
            f.single_only = found.kept_underlines.contains(&j);
            let expanded = if options.extend {
                expanded_frame(&found, j, &f)
            } else {
                f.clone()
            };
            let text = translations.get(&p.id).map(|t| t.trim()).unwrap_or("");
            let obstacles = obstacles_of(&found, j);
            let color = background(&p.rect, &bitmap);
            let safe = |setting: &Setting| {
                let r = &setting.drawn;
                r.bottom <= (found.scan.frame.height - 36.0).max(p.rect.bottom)
                    && r.right <= (found.scan.frame.width - 36.0).max(p.rect.right)
                    && (options.extend || r.bottom <= p.rect.bottom + 0.5)
                    && clear_expansion(&p.rect, r, &obstacles)
                    && clear_lines(setting, color, &bitmap)
            };
            let mut attempts = Vec::new();
            let (status, setting) = if p.status != Status::Translate {
                (p.status, None)
            } else if found.blocks[j]
                .lines
                .iter()
                .flat_map(|l| &l.items)
                .any(|it| found.scan.origins[it.id].unsafe_paint)
            {
                (Status::Retain(Retain::Paint), None)
            } else if text.is_empty() {
                (Status::Retain(Retain::Missing), None)
            } else if !text.chars().all(|c| face.has(c)) {
                (Status::Retain(Retain::Glyph), None)
            } else {
                let (setting, tried) = fit(
                    &atoms(text),
                    &face,
                    &f,
                    &expanded,
                    floor(&f, options.min_scale).max(1.0),
                    floor(&f, options.min_scale),
                    safe,
                );
                attempts = tried;
                match setting {
                    Some(s) => (Status::Translate, Some(s)),
                    None => (Status::Retain(Retain::Overflow), None),
                }
            };
            statuses.push(status);
            settings.push(setting);
            frames.push(f);
            expanded_frames.push(expanded);
            fit_attempts.push(attempts);
        }
        if options.uniform {
            // A short differently coloured prefix must not define a paragraph's
            // style. Count real source characters across every run.
            let style = |j: usize| majority_style(&found, j);
            let mut groups: HashMap<_, Vec<usize>> = HashMap::new();
            let mut paired = HashSet::new();
            let mut pairs = Vec::new();
            for (j, a) in found.paragraphs.iter().enumerate() {
                if settings[j].is_none() || a.lines != 1 {
                    continue;
                }
                for (k, b) in found.paragraphs.iter().enumerate() {
                    if settings[j].is_some()
                        && settings[k].is_some()
                        && !paired.contains(&j)
                        && !paired.contains(&k)
                        && a.lines == 1
                        && layout::fixed1(a.size) == layout::fixed1(b.size)
                        && hanging_continuation(&a.rect, &b.rect, a.size)
                    {
                        pairs.push(vec![j, k]);
                        paired.extend([j, k]);
                    }
                }
            }
            for (j, s) in settings.iter().enumerate() {
                if s.is_some() && !paired.contains(&j) {
                    groups.entry(style(j)).or_default().push(j);
                }
            }
            let mut groups: Vec<(Vec<usize>, bool)> =
                groups.into_values().map(|g| (g, false)).collect();
            groups.extend(pairs.into_iter().map(|g| (g, true)));
            let mut bullets: Vec<Vec<usize>> = Vec::new();
            for (j, p) in found.paragraphs.iter().enumerate() {
                if settings[j].is_none() || !bullet_item(&p.text) {
                    continue;
                }
                if let Some(group) = bullets.iter_mut().find(|g| {
                    let k = *g.last().unwrap();
                    let prev = &found.paragraphs[k];
                    style(j) == style(k)
                        && (p.rect.left - prev.rect.left).abs() <= p.size
                        && p.rect.top >= prev.rect.bottom - 1.0
                        && p.rect.top - prev.rect.bottom <= 3.0 * p.size
                }) {
                    group.push(j);
                } else {
                    bullets.push(vec![j]);
                }
            }
            groups.extend(bullets.into_iter().map(|g| (g, true)));
            for (group, siblings) in groups.iter().filter(|(g, _)| g.len() > 1) {
                let mut scales: Vec<_> = group
                    .iter()
                    .filter(|&&j| *siblings || uniform_anchor(&found, j))
                    .map(|&j| settings[j].as_ref().unwrap().scale)
                    .collect();
                if scales.is_empty() {
                    continue;
                }
                scales.sort_by(f64::total_cmp);
                let minimum = scales[0];
                let close_siblings = *siblings
                    && minimum >= 0.75 - 0.00001
                    && scales.last().unwrap() - minimum > 0.15 + 0.00001;
                let median = if close_siblings {
                    minimum
                } else if scales.len() < 3 {
                    *scales.last().unwrap()
                } else {
                    (scales[(scales.len() - 1) / 2] + scales[scales.len() / 2]) / 2.0
                };
                let mut replacements = Vec::new();
                for &j in group {
                    let p = &found.paragraphs[j];
                    let own = settings[j].as_ref().unwrap().scale;
                    let shared = own
                        .min(if close_siblings {
                            minimum
                        } else {
                            median.max(own - 0.10)
                        })
                        .max(floor(&frames[j], options.min_scale));
                    if shared >= own - 0.00001 {
                        continue;
                    }
                    let obstacles = obstacles_of(&found, j);
                    let color = background(&p.rect, &bitmap);
                    let (again, mut tried) = fit(
                        &atoms(translations[&p.id].trim()),
                        &face,
                        &frames[j],
                        &expanded_frames[j],
                        shared,
                        shared,
                        |setting| {
                            let r = &setting.drawn;
                            r.bottom <= (found.scan.frame.height - 36.0).max(p.rect.bottom)
                                && r.right <= (found.scan.frame.width - 36.0).max(p.rect.right)
                                && (options.extend || r.bottom <= p.rect.bottom + 0.5)
                                && clear_expansion(&p.rect, r, &obstacles)
                                && clear_lines(setting, color, &bitmap)
                        },
                    );
                    if let Some(mut again) = again {
                        again.stage = FitStage::Uniform;
                        if let Some(attempt) = tried.last_mut() {
                            attempt.stage = FitStage::Uniform;
                        }
                        replacements.push((j, again));
                    }
                    fit_attempts[j].extend(tried);
                }
                // Use the largest common feasible leading, never exceeding each
                // source's leading. Check each intermediate candidate for rules.
                for &j in group {
                    if !replacements.iter().any(|(k, _)| *k == j) {
                        replacements.push((j, settings[j].as_ref().unwrap().clone()));
                    }
                }
                let common = replacements
                    .iter()
                    .filter(|(_, s)| s.lines.len() > 1)
                    .map(|(_, s)| s.line_height / s.size)
                    .fold(f64::INFINITY, f64::min);
                for (j, mut s) in replacements {
                    if common.is_finite() && s.lines.len() > 1 {
                        let ratio = common.min(frames[j].line_height);
                        let delta = s.line_height - ratio * s.size;
                        for (k, ((_, _, y), r)) in
                            s.lines.iter_mut().zip(&mut s.line_rects).enumerate()
                        {
                            *y -= k as f64 * delta;
                            r.top -= k as f64 * delta;
                            r.bottom -= k as f64 * delta;
                        }
                        s.drawn.bottom = s.line_rects.last().unwrap().bottom;
                        s.line_height = ratio * s.size;
                    }
                    if settings[j]
                        .as_ref()
                        .is_some_and(|old| old.lines == s.lines && old.size == s.size)
                    {
                        continue;
                    }
                    let p = &found.paragraphs[j];
                    if clear_lines(&s, background(&p.rect, &bitmap), &bitmap) {
                        fit_attempts[j].push(FitAttempt {
                            stage: FitStage::Uniform,
                            scale: s.scale,
                            line_height: s.line_height,
                            fits: true,
                            rejection: None,
                            candidate: Some(s.drawn),
                        });
                        settings[j] = Some(s);
                    }
                }
            }
        }
        // Two independent expansions may claim the same whitespace. Resolve before
        // erasure; repeat because a rejected setting restores its full source box.
        let mut retried_original = HashSet::new();
        loop {
            let mut rejected = HashSet::new();
            for (j, p) in found.paragraphs.iter().enumerate() {
                let Some(s) = &settings[j] else { continue };
                for (k, other) in found.paragraphs.iter().enumerate() {
                    if j == k {
                        continue;
                    }
                    let r = settings[k].as_ref().map_or(other.rect, |s| s.drawn);
                    let translated_collision = settings[k].as_ref().is_some_and(|other| {
                        s.line_rects.iter().any(|a| {
                            other.line_rects.iter().any(|b| {
                                let (w, h) = overlap(a, b);
                                w > 0.5 && h > 0.5
                            })
                        })
                    });
                    if translated_collision || !clear_expansion(&p.rect, &s.drawn, &[r]) {
                        rejected.insert(j);
                    }
                }
            }
            // Restore all sources rejected so far before the final pixel pass.
            let restore: Vec<_> = taken
                .iter()
                .copied()
                .filter(|(j, _)| settings[*j].is_none())
                .collect();
            if !restore.is_empty() {
                taken.retain(|(j, _)| settings[*j].is_some());
                for (_, object) in restore {
                    lib.FPDFPage_InsertObject(graphics_page.raw, object);
                }
                bitmap = render(lib, &graphics_page, AUDIT_SCALE);
            }
            for (j, s) in settings.iter().enumerate() {
                if let Some(s) = s
                    && !clear_lines(s, background(&found.paragraphs[j].rect, &bitmap), &bitmap)
                {
                    rejected.insert(j);
                }
            }
            if rejected.is_empty() {
                break;
            }
            let mut rejected: Vec<_> = rejected.into_iter().collect();
            rejected.sort_unstable();
            let mut changed_setting = false;
            for &j in &rejected {
                if !retried_original.insert(j) {
                    continue;
                }
                let p = &found.paragraphs[j];
                let mut original = frames[j].clone();
                original.limit = p.rect.bottom;
                original.narrow_limit = p.rect.bottom;
                let obstacles = obstacles_of(&found, j);
                // The bitmap does not change during either fit search.
                let color = background(&p.rect, &bitmap);
                // A neighbour may have expanded since the individual fit. First
                // retry at the same size against its actual final placement.
                let own_scale = settings[j].as_ref().unwrap().scale;
                let (nearby, tried) = fit(
                    &atoms(translations[&p.id].trim()),
                    &face,
                    &frames[j],
                    &expanded_frames[j],
                    own_scale,
                    own_scale,
                    |s| {
                        s.drawn.bottom <= (found.scan.frame.height - 36.0).max(p.rect.bottom)
                            && s.drawn.right <= (found.scan.frame.width - 36.0).max(p.rect.right)
                            && (options.extend || s.drawn.bottom <= p.rect.bottom + 0.5)
                            && clear_expansion(&p.rect, &s.drawn, &obstacles)
                            && clear_lines(s, color, &bitmap)
                            && settings.iter().enumerate().all(|(k, other)| {
                                if k == j {
                                    return true;
                                }
                                let r = other
                                    .as_ref()
                                    .map_or(found.paragraphs[k].rect, |other| other.drawn);
                                clear_expansion(&p.rect, &s.drawn, &[r])
                                    && other.as_ref().is_none_or(|other| {
                                        !s.line_rects.iter().any(|a| {
                                            other.line_rects.iter().any(|b| {
                                                let (w, h) = overlap(a, b);
                                                w > 0.5 && h > 0.5
                                            })
                                        })
                                    })
                            })
                    },
                );
                fit_attempts[j].extend(tried);
                if let Some(nearby) = nearby {
                    settings[j] = Some(nearby);
                    changed_setting = true;
                    continue;
                }
                let (retry, attempts) = fit(
                    &atoms(translations[&p.id].trim()),
                    &face,
                    &original,
                    &original,
                    settings[j].as_ref().unwrap().scale,
                    floor(&original, options.min_scale),
                    |s| {
                        s.drawn.bottom <= p.rect.bottom + 0.5
                            && s.drawn.right <= p.rect.right + 0.5
                            && clear_expansion(&p.rect, &s.drawn, &obstacles)
                            && clear_lines(s, color, &bitmap)
                            && settings.iter().enumerate().all(|(k, other)| {
                                k == j
                                    || rejected.contains(&k)
                                    || other.as_ref().is_none_or(|other| {
                                        !s.line_rects.iter().any(|a| {
                                            other.line_rects.iter().any(|b| {
                                                let (w, h) = overlap(a, b);
                                                w > 0.5 && h > 0.5
                                            })
                                        })
                                    })
                            })
                    },
                );
                fit_attempts[j].extend(attempts);
                if let Some(retry) = retry {
                    settings[j] = Some(retry);
                    changed_setting = true;
                    // Retry both members before testing the pair again.
                }
            }
            if changed_setting {
                continue;
            }
            for j in rejected {
                settings[j] = None;
                statuses[j] = Status::Retain(Retain::Overflow);
            }
        }
        for (_, object) in taken {
            lib.FPDFPageObj_Destroy(object);
        }
        // Remove the source text of the paragraphs translated, then draw their translations.
        let mut removed: HashSet<usize> = HashSet::new();
        let mut changed = false;
        for (j, block) in found.blocks.iter().enumerate() {
            if settings[j].is_none() {
                continue;
            }
            if matches!(mode, WriteMode::Plan) {
                characters.extend(selected_characters(lib, &page, &found, j, false)?);
                continue;
            }
            // All of its objects, or none: were one not to come off, those taken are put back
            // (on top of the page: the order of drawing may change) and the source is kept.
            let mut taken = Vec::new();
            let mut failed = false;
            for object in paragraph_objects(&found, j)
                .into_iter()
                .filter(|_| matches!(mode, WriteMode::Pdfium))
            {
                if object == 0 || removed.contains(&object) || taken.contains(&object) {
                    continue;
                }
                if lib.FPDFPage_RemoveObject(page.raw, object as FPDF_PAGEOBJECT) == 0 {
                    failed = true;
                    break;
                }
                taken.push(object);
            }
            if failed {
                for &object in &taken {
                    lib.FPDFPage_InsertObject(page.raw, object as FPDF_PAGEOBJECT);
                }
                changed |= !taken.is_empty();
                statuses[j] = Status::Retain(Retain::Erase);
                settings[j] = None;
                continue;
            }
            for &object in &taken {
                lib.FPDFPageObj_Destroy(object as FPDF_PAGEOBJECT);
                removed.insert(object);
            }
            changed = true;
            if erase_only {
                continue;
            }
            let Some(setting) = &settings[j] else {
                continue;
            };
            let first = block
                .lines
                .iter()
                .flat_map(|l| &l.items)
                .find(|it| found.scan.origins[it.id].object != 0)
                .map_or(0, |it| it.id);
            // Text only: no path is drawn with a translation.
            let color = found.scan.origins[first].color;
            #[cfg(not(feature = "pdf-erase"))]
            let [r, g, b, a] = color;
            for (s, x, y) in &setting.lines {
                #[cfg(feature = "pdf-erase")]
                translation_lines.push(super::unicode::TextLine {
                    page: i,
                    text: s.clone(),
                    size: setting.size,
                    matrix: found.scan.frame.text_matrix(*x, *y),
                    color,
                });
                #[cfg(not(feature = "pdf-erase"))]
                {
                    let object = lib.FPDFPageObj_CreateTextObj(
                        destination.raw,
                        font.raw,
                        setting.size as f32,
                    );
                    if object.is_null() {
                        return Err(internal());
                    }
                    lib.FPDFText_SetText_str(object, s);
                    let [ma, mb, mc, md, me, mf] = found.scan.frame.text_matrix(*x, *y);
                    lib.FPDFPageObj_Transform(object, ma, mb, mc, md, me, mf);
                    lib.FPDFPageObj_SetFillColor(object, r, g, b, a);
                    lib.FPDFPage_InsertObject(target.raw, object);
                }
            }
            changed = true;
        }
        if changed
            && (!cfg!(feature = "pdf-erase") || matches!(mode, WriteMode::Pdfium))
            && lib.FPDFPage_GenerateContent(target.raw) == 0
        {
            return Err(internal());
        }
        let source_styles: Vec<_> = (0..found.paragraphs.len())
            .map(|j| majority_style(&found, j))
            .collect();
        for (j, p) in found.paragraphs.into_iter().enumerate() {
            let setting = settings[j].take();
            outcomes.push(Outcome {
                source_style: source_styles[j].clone(),
                translation: (statuses[j] == Status::Translate)
                    .then(|| translations[&p.id].trim().to_owned()),
                status: statuses[j],
                scale: setting.as_ref().map(|s| s.scale),
                fit_stage: setting.as_ref().map(|s| s.stage),
                line_height: setting.as_ref().map(|s| s.line_height),
                attempts: std::mem::take(&mut fit_attempts[j]),
                drawn_lines: setting.as_ref().map_or(0, |s| s.lines.len()),
                line_rects: setting
                    .as_ref()
                    .map_or_else(Vec::new, |s| s.line_rects.clone()),
                drawn: setting.as_ref().map(|s| s.drawn),
                wrapped: setting.as_ref().is_some_and(|s| s.wrapped),
                paragraph: p,
            });
        }
    }
    let out = if matches!(mode, WriteMode::Plan) {
        Vec::new()
    } else if cfg!(feature = "pdf-erase")
        && let WriteMode::Erased(clean) = mode
    {
        clean.to_vec()
    } else {
        destination.save()?
    };
    #[cfg(feature = "pdf-erase")]
    let out = super::unicode::append_faces(&out, &face.fonts, &translation_lines, false)?;
    if erase_only {
        for o in &mut outcomes {
            o.translation = (o.status == Status::Translate).then(String::new);
            o.drawn = None;
            o.drawn_lines = 0;
            o.line_rects.clear();
        }
    }
    #[cfg(not(feature = "pdf-erase"))]
    drop(font);
    Ok((out, outcomes, characters))
}

fn selected_characters(
    lib: &dyn PdfiumLibraryBindings,
    page: &Page<'_>,
    found: &Found,
    j: usize,
    within: bool,
) -> Result<Vec<EraseChar>, Error> {
    let block = &found.blocks[j];
    let mut characters = Vec::new();
    let objects: HashSet<usize> = block
        .lines
        .iter()
        .flat_map(|l| &l.items)
        .map(|it| found.scan.origins[it.id].object)
        .collect();
    let text = TextPage {
        lib,
        raw: lib.FPDFText_LoadPage(page.raw),
    };
    if text.raw.is_null() {
        return Err(internal());
    }
    for index in 0..lib.FPDFText_CountChars(text.raw) {
        if lib.FPDFText_IsGenerated(text.raw, index) == 1
            || !objects.contains(&(lib.FPDFText_GetTextObject(text.raw, index) as usize))
        {
            continue;
        }
        let (mut x, mut y) = (0.0, 0.0);
        let (mut l, mut r, mut b, mut t) = (0.0, 0.0, 0.0, 0.0);
        if lib.FPDFText_GetCharOrigin(text.raw, index, &mut x, &mut y) == 0
            || lib.FPDFText_GetCharBox(text.raw, index, &mut l, &mut r, &mut b, &mut t) == 0
        {
            return Err(internal());
        }
        let (sx, sy) = found.scan.frame.down(x, y);
        if within
            && !block.lines.iter().flat_map(|l| &l.items).any(|it| {
                found.scan.origins[it.id].object
                    == lib.FPDFText_GetTextObject(text.raw, index) as usize
                    && (it.y - sy).abs() < 0.25 * it.size
                    && sx >= it.x - 0.1
                    && sx < it.x + it.w - 0.01
            })
        {
            continue;
        }
        characters.push(EraseChar {
            paragraph: found.paragraphs[j].id.clone(),
            page: found.paragraphs[j].page,
            index,
            unicode: lib.FPDFText_GetUnicode(text.raw, index),
            is_hyphen: lib.FPDFText_IsHyphen(text.raw, index) == 1,
            origin: (x, y),
            rect: found.scan.frame.rect(l, b, r, t),
        });
    }
    Ok(characters)
}

/// Feasibility experiment for paragraphs previously blocked by shared text objects.
/// No translation is invented; this selects source characters for erasure alone.
pub fn shared_candidates(bytes: &[u8], pages: Option<usize>) -> Result<Vec<EraseChar>, Error> {
    let (_guard, pdfium) = pdfium()?;
    let doc = read_doc(&pdfium, bytes)?;
    let (body, leading) = document_body(&doc)?;
    let mut out = Vec::new();
    for i in 0..page_limit(&doc, pages) {
        let page = doc.page(i)?;
        let found = verified_found(doc.lib, &page, i, body, leading);
        for (j, p) in found.paragraphs.iter().enumerate() {
            if p.status == Status::Retain(Retain::Shared) {
                out.extend(selected_characters(doc.lib, &page, &found, j, true)?);
            }
        }
    }
    Ok(out)
}

/// The pixel rows at `scale` of a page from `bottom` up to `top` (PDF user space).
#[cfg(test)]
pub(crate) fn render_band(
    bytes: &[u8],
    page: usize,
    scale: f64,
    bottom: f64,
    top: f64,
) -> Result<Vec<u8>, Error> {
    let (_guard, pdfium) = pdfium()?;
    let doc = Doc::load(pdfium.bindings(), bytes)?;
    let page = doc.page(page)?;
    let height = Frame::of(&page).height;
    let (w, h, pixels) = render(doc.lib, &page, scale);
    let rows = |y: f64| ((height - y) * scale).round().clamp(0.0, h as f64) as usize;
    Ok(pixels[rows(top) * w * 4..rows(bottom) * w * 4].to_vec())
}

/// Whether the one text object on the first page of `probe` paints nothing at all, in a
/// font program that PDFium read from the file (not one it substituted for it).
pub(super) fn paints_nothing(probe: &[u8]) -> Result<bool, Error> {
    let (_guard, pdfium) = pdfium()?;
    let doc = Doc::load(pdfium.bindings(), probe)?;
    let lib = doc.lib;
    let page = doc.page(0)?;
    if lib.FPDFPage_CountObjects(page.raw) != 1 {
        return Ok(false);
    }
    let object = lib.FPDFPage_GetObject(page.raw, 0);
    if object.is_null() || lib.FPDFPageObj_GetType(object) != PAGEOBJ_TEXT {
        return Ok(false);
    }
    let font = lib.FPDFTextObj_GetFont(object);
    if font.is_null() || lib.FPDFFont_GetIsEmbedded(font) != 1 {
        return Ok(false);
    }
    let (w, h, pixels) = render(lib, &page, 1.0);
    Ok(w > 0
        && h > 0
        && pixels
            .as_chunks::<4>()
            .0
            .iter()
            .all(|p| p[..3] == [255, 255, 255]))
}

/// Raw, nongenerated source characters for independent erasure checks and reader comparison.
/// No text is logged; consumers store this only with the private experiment artifacts.
pub fn characters(bytes: &[u8], pages: Option<usize>) -> Result<Vec<EraseChar>, Error> {
    characters_on_pages(bytes, pages, None)
}

pub(super) fn characters_on_pages(
    bytes: &[u8],
    pages: Option<usize>,
    selected: Option<&HashSet<usize>>,
) -> Result<Vec<EraseChar>, Error> {
    let (_guard, pdfium) = pdfium()?;
    let doc = read_doc(&pdfium, bytes)?;
    let mut out = Vec::new();
    for page in 0..page_limit(&doc, pages) {
        if selected.is_some_and(|set| !set.contains(&page)) {
            continue;
        }
        let p = doc.page(page)?;
        let frame = Frame::of(&p);
        let lib = doc.lib;
        let text = TextPage {
            lib,
            raw: lib.FPDFText_LoadPage(p.raw),
        };
        if text.raw.is_null() {
            return Err(internal());
        }
        for index in 0..lib.FPDFText_CountChars(text.raw) {
            if lib.FPDFText_IsGenerated(text.raw, index) == 1 {
                continue;
            }
            let (mut x, mut y, mut l, mut r, mut b, mut t) = (0., 0., 0., 0., 0., 0.);
            if lib.FPDFText_GetCharOrigin(text.raw, index, &mut x, &mut y) == 0
                || lib.FPDFText_GetCharBox(text.raw, index, &mut l, &mut r, &mut b, &mut t) == 0
            {
                return Err(internal());
            }
            out.push(EraseChar {
                paragraph: String::new(),
                page,
                index,
                unicode: lib.FPDFText_GetUnicode(text.raw, index),
                is_hyphen: lib.FPDFText_IsHyphen(text.raw, index) == 1,
                origin: (x, y),
                rect: frame.rect(l, b, r, t),
            });
        }
    }
    Ok(out)
}

// Only a marked extraction character can stand for either source hyphen encoding.
pub(crate) fn matches_source_unicode(character: &EraseChar, source: u32) -> bool {
    character.unicode == source
        || (character.is_hyphen && character.unicode == 2 && matches!(source, 45 | 173))
}

/// Non-generated characters must preserve both identity and origin (1e-5 pt numerical tolerance).
pub(crate) fn same_character(a: &EraseChar, b: &EraseChar) -> bool {
    (matches_source_unicode(a, b.unicode) || matches_source_unicode(b, a.unicode))
        && (a.origin.0 - b.origin.0).abs() <= 0.00001
        && (a.origin.1 - b.origin.1).abs() <= 0.00001
}

pub(crate) fn same_characters<'a>(
    mut expected: impl Iterator<Item = &'a EraseChar>,
    mut actual: impl Iterator<Item = &'a EraseChar>,
) -> bool {
    loop {
        match (expected.next(), actual.next()) {
            (None, None) => return true,
            (Some(a), Some(b)) if same_character(a, b) => (),
            _ => return false,
        }
    }
}

/// Exact remaining character sequence and pixel masks of the erased glyphs (not paragraphs).
pub fn check_erasure(
    original: &[u8],
    output: &[u8],
    erased: &[EraseChar],
    pages: Option<usize>,
) -> Result<(Vec<usize>, Vec<PageCheck>), Error> {
    let a = characters(original, pages)?;
    let b = characters(output, pages)?;
    let selected: HashSet<_> = erased.iter().map(|c| (c.page, c.index)).collect();
    let (_guard, pdfium) = pdfium()?;
    let da = Doc::load(pdfium.bindings(), original)?;
    let db = Doc::load(pdfium.bindings(), output)?;
    let mut mismatches = Vec::new();
    let mut pixels = Vec::new();
    for page in 0..page_limit(&da, pages) {
        let expected: Vec<_> = a
            .iter()
            .filter(|c| c.page == page && !selected.contains(&(page, c.index)))
            .collect();
        let actual: Vec<_> = b.iter().filter(|c| c.page == page).collect();
        if !same_characters(expected.into_iter(), actual.into_iter()) {
            mismatches.push(page);
        }
        let (w, h, ra) = render(da.lib, &da.page(page)?, 1.25);
        let (w2, h2, rb) = render(db.lib, &db.page(page)?, 1.25);
        let boxes = erase_boxes(erased, page, 1.25);
        let (outside, inside) = if (w, h) == (w2, h2) {
            changed_pixels(&ra, &rb, w, h, 4, &boxes)
        } else {
            (1., 0.)
        };
        pixels.push(PageCheck {
            page,
            changed_outside: outside,
            changed_inside: inside,
            darkened_outside: if (w, h) == (w2, h2) {
                pixel_changes(&ra, &rb, w, h, 4, &boxes).2
            } else {
                w * h
            },
            lightened_outside: if (w, h) == (w2, h2) {
                lost_ink(&ra, &rb, w, h, 4, &boxes)
            } else {
                w * h
            },
            // Counted by the save's check ([`check`]), not by this erasure-only one.
            paths_changed: 0,
        });
    }
    Ok((mismatches, pixels))
}

/// Tight glyph boxes expanded by two pixels at 90 dpi, scaled with the rendering resolution.
pub fn erase_boxes(chars: &[EraseChar], page: usize, scale: f64) -> Vec<Rect> {
    chars
        .iter()
        .filter(|c| c.page == page)
        .map(|c| Rect {
            left: c.rect.left * scale - 2.0 * scale / 1.25,
            top: c.rect.top * scale - 2.0 * scale / 1.25,
            right: c.rect.right * scale + 2.0 * scale / 1.25,
            bottom: c.rect.bottom * scale + 2.0 * scale / 1.25,
        })
        .collect()
}

// ---- Seeing the paragraphs ----

/// The PDF with each paragraph's box drawn over it: green to translate, orange kept (a formula,
/// shared or nested text), gray not worth translating; formulas inside red. Each box is labeled
/// with its id.
pub fn trace(bytes: &[u8], pages: Option<usize>) -> Result<Vec<u8>, Error> {
    let analysis = analyze(bytes, pages)?;
    let (_guard, pdfium) = pdfium()?;
    let doc = Doc::load(pdfium.bindings(), bytes)?;
    let lib = doc.lib;
    let label = Font {
        lib,
        raw: lib.FPDFText_LoadStandardFont(doc.raw, "Helvetica"),
    };
    for info in &analysis.analyzed {
        let Some(first) = info.paragraphs.first() else {
            continue;
        };
        let page = doc.page(first.page)?;
        let frame = Frame::of(&page);
        let rect = |r: &Rect, color: [u32; 3], width: f64| {
            let (x, y, w, h) = frame.page_rect(r);
            let o = lib.FPDFPageObj_CreateNewRect(x as f32, y as f32, w as f32, h as f32);
            if o.is_null() {
                return;
            }
            lib.FPDFPageObj_SetStrokeColor(o, color[0], color[1], color[2], 255);
            lib.FPDFPageObj_SetStrokeWidth(o, width as f32);
            lib.FPDFPath_SetDrawMode(o, 0, 1);
            lib.FPDFPage_InsertObject(page.raw, o);
        };
        for p in &info.paragraphs {
            let color = match p.status {
                Status::Translate => [0, 150, 60],
                Status::Retain(_) => [230, 120, 0],
                Status::Skip(_) => [150, 150, 150],
            };
            rect(&p.rect, color, 0.6);
            for m in &p.maths {
                rect(m, [220, 0, 0], 0.4);
            }
            if !label.raw.is_null() {
                let o = lib.FPDFPageObj_CreateTextObj(doc.raw, label.raw, 4.5);
                if !o.is_null() {
                    let tag = match p.status.reason() {
                        Some(r) => format!("{} {r}", p.id),
                        None => p.id.clone(),
                    };
                    lib.FPDFText_SetText_str(o, &tag);
                    let [ma, mb, mc, md, me, mf] = frame.text_matrix(p.rect.left, p.rect.top - 0.8);
                    lib.FPDFPageObj_Transform(o, ma, mb, mc, md, me, mf);
                    lib.FPDFPageObj_SetFillColor(o, color[0], color[1], color[2], 255);
                    lib.FPDFPage_InsertObject(page.raw, o);
                }
            }
        }
        lib.FPDFPage_GenerateContent(page.raw);
    }
    let out = doc.save();
    drop(label);
    out
}

// ---- Checking a write ----

#[derive(Clone, Debug, Default)]
pub struct PageCheck {
    /// From 0.
    pub page: usize,
    /// Pixels changed outside the boxes of the paragraphs translated, of all the page's.
    pub changed_outside: f64,
    /// Pixels changed inside them, of all the page's.
    pub changed_inside: f64,
    /// Outside pixels with any RGB channel darkening (including one-level changes).
    pub darkened_outside: usize,
    /// Outside pixels, and outside the glyphs erased with the paragraphs translated
    /// ([`kept_boxes`]), with a color lighter by more than 24: ink lost where nothing was to be
    /// erased (any is too many: a dot or a rule is a few pixels of a page).
    pub lightened_outside: usize,
    /// Paths of the page (on it and in its Forms) not drawn as they were (in paint, clip,
    /// order), or drawn that were not ([`paths_changed`]), with none let pass: a translation is
    /// text only, and no path is ever taken off. Any is too many.
    pub paths_changed: usize,
}

/// What the text of the pages tells, as one reader reads it: PDFium in [`check`], another
/// (poppler) in the developer command, through [`check_text`].
#[derive(Clone, Debug, Default)]
pub struct TextCheck {
    /// Paragraphs whose translation is not found in the output's text, as many times as the
    /// translations of the page (and the text kept) hold it.
    pub missing: Vec<String>,
    /// Of `missing`, those found once the characters that share a glyph in the output font are
    /// taken as one ([`same_glyph`]): drawn right, but read back as another character.
    pub substituted: Vec<String>,
    /// Paragraphs translated whose source text is still in the output's text.
    pub remaining: Vec<String>,
    /// Paragraphs translated with a line not found in the source's own text, which cannot be
    /// checked for (not counted in `remaining`).
    pub unverified: Vec<String>,
    /// Paragraphs kept as drawn (retained or skipped) with a line of the source's text that is
    /// no longer in the output's.
    pub lost: Vec<String>,
    /// Paragraphs kept with a line not found in the source's own text (not counted in `lost`).
    pub kept_unverified: Vec<String>,
    /// Paragraphs with a line found in the source's text only where another paragraph's line
    /// is: where it is cannot be told, so it is not checked (and does not pass).
    pub unplaced: Vec<String>,
}

#[derive(Clone, Debug, Default)]
pub struct Check {
    pub pages_original: usize,
    pub pages_output: usize,
    pub pages: Vec<PageCheck>,
    pub text: TextCheck,
    /// The paragraphs of the source, analyzed again for the check.
    pub extracted: usize,
    /// Of those, the ones that no outcome is for.
    pub unaccounted: Vec<String>,
    /// Outcomes for no paragraph of the source, or for one more than once.
    pub unknown: Vec<String>,
    /// A translation drawn over another paragraph: its id, and the other's. Only where the two
    /// boxes overlap more than the source's did.
    pub overlaps: Vec<(String, String)>,
    /// Translations drawn over what is in no paragraph ([`PageInfo::obstacles`]), further than
    /// their sources were.
    pub crossings: Vec<String>,
    /// Content remaining beneath a translated line on the erased page (144 dpi).
    pub occupied_lines: Vec<String>,
    /// A translated line that did not visibly change any erased-page pixel.
    pub invisible_lines: Vec<String>,
    pub translated: usize,
    pub retained: usize,
    pub skipped: usize,
}

impl Check {
    /// The largest share of pixels changed outside the paragraphs on any page.
    pub fn worst_outside(&self) -> f64 {
        self.pages
            .iter()
            .map(|p| p.changed_outside)
            .fold(0.0, f64::max)
    }

    /// Whether the paragraphs written are those of the source, each once.
    pub fn accounted(&self) -> bool {
        self.unaccounted.is_empty()
            && self.unknown.is_empty()
            && self.translated + self.retained + self.skipped == self.extracted
    }
}

// Read per-character Unicode to retain IsHyphen provenance. PDFium can return UTF-16
// surrogate units; decode the complete sequence rather than treating each as a scalar.
fn raw_page_text(lib: &dyn PdfiumLibraryBindings, page: &Page<'_>) -> String {
    let raw = lib.FPDFText_LoadPage(page.raw);
    if raw.is_null() {
        return String::new();
    }
    let text = TextPage { lib, raw };
    let mut units = Vec::new();
    for index in 0..lib.FPDFText_CountChars(text.raw) {
        let code = lib.FPDFText_GetUnicode(text.raw, index);
        if code == 2 && lib.FPDFText_IsHyphen(text.raw, index) == 1 {
            units.push(45);
        } else if code <= 0xffff {
            units.push(code as u16);
        } else if let Some(ch) = char::from_u32(code) {
            units.extend_from_slice(ch.encode_utf16(&mut [0; 2]));
        }
    }
    String::from_utf16_lossy(&units)
}

fn page_text(lib: &dyn PdfiumLibraryBindings, page: &Page<'_>) -> String {
    raw_page_text(lib, page)
        .chars()
        .filter(|c| !c.is_whitespace())
        .collect()
}

/// The page drawn at `scale` (pixels per point), as BGRx rows.
fn render(lib: &dyn PdfiumLibraryBindings, page: &Page<'_>, scale: f64) -> (usize, usize, Vec<u8>) {
    let frame = Frame::of(page);
    let w = (frame.width * scale).round().max(1.0) as c_int;
    let h = (frame.height * scale).round().max(1.0) as c_int;
    let bitmap = lib.FPDFBitmap_Create(w, h, 0);
    if bitmap.is_null() {
        return (0, 0, Vec::new());
    }
    lib.FPDFBitmap_FillRect(bitmap, 0, 0, w, h, 0xFFFF_FFFF);
    lib.FPDF_RenderPageBitmap(bitmap, page.raw, 0, 0, w, h, 0, RENDER_ANNOTATIONS);
    let stride = lib.FPDFBitmap_GetStride(bitmap) as usize;
    let data = lib.FPDFBitmap_GetBuffer(bitmap).cast::<u8>();
    let mut out = vec![0u8; w as usize * h as usize * 4];
    for row in 0..h as usize {
        // SAFETY: the bitmap holds `h` rows of `stride` bytes, each at least `4 * w`.
        let src = unsafe { std::slice::from_raw_parts(data.add(row * stride), w as usize * 4) };
        out[row * w as usize * 4..(row + 1) * w as usize * 4].copy_from_slice(src);
    }
    lib.FPDFBitmap_Destroy(bitmap);
    (w as usize, h as usize, out)
}

/// Characters that share one glyph in IPA Gothic, so that the font PDFium embeds reads either
/// back as the other (its ToUnicode map takes one of them): white space, hyphen and soft
/// hyphen, tildes and wave dashes, dashes, full-width signs, and one pair of ideographs.
pub fn same_glyph(ch: char) -> char {
    match ch {
        '\u{A0}' => ' ',
        '\u{AD}' => '-',
        '\u{2DC}' => '~',
        '\u{FFE0}' => '¢',
        '\u{FFE1}' => '£',
        '\u{FFE5}' => '¥',
        '\u{FFE2}' => '¬',
        '\u{FF65}' => '·',
        '\u{2015}' => '—',
        '\u{FFE3}' => '‾',
        '\u{FF5E}' => '\u{301C}',
        '\u{29FD7}' => '\u{29FCE}',
        _ => ch,
    }
}

fn fold(s: &str) -> String {
    s.chars().map(same_glyph).collect()
}

fn count(haystack: &str, needle: &str) -> usize {
    if needle.is_empty() {
        0
    } else {
        haystack.matches(needle).count()
    }
}

/// The width and height of where two boxes overlap (0 where they do not).
fn overlap(a: &Rect, b: &Rect) -> (f64, f64) {
    let w = a.right.min(b.right) - a.left.max(b.left);
    let h = a.bottom.min(b.bottom) - a.top.max(b.top);
    if w > 0.0 && h > 0.0 {
        (w, h)
    } else {
        (0.0, 0.0)
    }
}

/// Translations drawn over another paragraph of their page (its translation, or its source text
/// kept): where the boxes overlap by more than 1 pt each way, and by more than 1 pt wider or
/// higher than their sources' did (a translation may end up to 0.5 pt below its box).
pub fn overlaps(outcomes: &[&Outcome]) -> Vec<(String, String)> {
    let mut out = Vec::new();
    for a in outcomes {
        let Some(da) = a.drawn.filter(|_| a.status == Status::Translate) else {
            continue;
        };
        for b in outcomes {
            if std::ptr::eq(*a, *b) || a.paragraph.page != b.paragraph.page {
                continue;
            }
            let db = match b.status {
                Status::Translate => b.drawn.unwrap_or(b.paragraph.rect),
                _ => b.paragraph.rect,
            };
            if b.status == Status::Translate
                && a.line_rects.iter().any(|a| {
                    b.line_rects.iter().any(|b| {
                        let (w, h) = overlap(a, b);
                        w > 0.5 && h > 0.5
                    })
                })
            {
                out.push((a.paragraph.id.clone(), b.paragraph.id.clone()));
                continue;
            }
            let (w, h) = overlap(&da, &db);
            let (w0, h0) = overlap(&a.paragraph.rect, &b.paragraph.rect);
            if w > 1.0 && h > 1.0 && (w > w0 + 1.0 || h > h0 + 1.0) {
                out.push((a.paragraph.id.clone(), b.paragraph.id.clone()));
            }
        }
    }
    out
}

/// How far `a` and `o` meet, width and height, where they do (`o` may be a line, of no width
/// or height).
fn meet(a: &Rect, o: &Rect) -> Option<(f64, f64)> {
    (a.left < o.right && a.right > o.left && a.top < o.bottom && a.bottom > o.top).then(|| {
        (
            a.right.min(o.right) - a.left.max(o.left),
            a.bottom.min(o.bottom) - a.top.max(o.top),
        )
    })
}

/// Translations drawn over any of `obstacles` (of their page): where the translation's box,
/// half a point in, meets one more than 1 pt wider or higher than its source's box did.
pub fn crossings(outcomes: &[&Outcome], obstacles: &[Rect]) -> Vec<String> {
    let mut out = Vec::new();
    for a in outcomes {
        let Some(d) = a.drawn.filter(|_| a.status == Status::Translate) else {
            continue;
        };
        let d = Rect {
            left: d.left + 0.5,
            right: d.right - 0.5,
            top: d.top + 0.5,
            bottom: d.bottom - 0.5,
        };
        let crossed = obstacles
            .iter()
            .any(|o| match (meet(&d, o), meet(&a.paragraph.rect, o)) {
                (Some(_), None) => true,
                (Some((w, h)), Some((w0, h0))) => w > w0 + 1.0 || h > h0 + 1.0,
                (None, _) => false,
            });
        if crossed {
            out.push(a.paragraph.id.clone());
        }
    }
    out
}

/// Where `needle` starts in `haystack`, overlapping ones too.
fn starts(haystack: &str, needle: &str) -> Vec<usize> {
    let mut out = Vec::new();
    if needle.is_empty() {
        return out;
    }
    let mut from = 0;
    while let Some(at) = haystack[from..].find(needle) {
        out.push(from + at);
        from += at
            + haystack[from + at..]
                .chars()
                .next()
                .map_or(1, char::len_utf8);
    }
    out
}

/// Where each line of the page's paragraphs is in the source's text: the lines found in fewer
/// places first, with longer lines first when counts tie. This reserves unambiguous lines
/// before considering matches that overlap them, such as a heading found across a page number
/// and the start of the next line. Returns `None` for lines with no unoccupied match.
fn place(source: &str, outcomes: &[&Outcome]) -> Vec<Vec<Option<(usize, usize)>>> {
    let mut placed: Vec<Vec<Option<(usize, usize)>>> = outcomes
        .iter()
        .map(|o| vec![None; o.paragraph.source_lines.len()])
        .collect();
    let mut order: Vec<(usize, usize, &str, Vec<usize>)> = outcomes
        .iter()
        .enumerate()
        .flat_map(|(i, o)| {
            o.paragraph
                .source_lines
                .iter()
                .enumerate()
                .filter(|(_, l)| !l.is_empty())
                .map(move |(k, l)| (i, k, l.as_str(), starts(source, l)))
        })
        .collect();
    order.sort_by_key(|(_, _, line, at)| (at.len(), std::cmp::Reverse(line.chars().count())));
    let mut taken = vec![false; source.len()];
    for (i, k, line, at) in order {
        let end = |s: usize| s + line.len();
        if let Some(&s) = at.iter().find(|&&s| !taken[s..end(s)].contains(&true)) {
            taken[s..end(s)].fill(true);
            placed[i][k] = Some((s, end(s)));
        }
    }
    placed
}

/// The times `needle` is in `haystack` without touching `spans` (taken one after the other, as
/// [`count`] does).
fn count_clear(haystack: &str, needle: &str, spans: &[(usize, usize)]) -> usize {
    let mut n = 0;
    let mut end = 0;
    for at in starts(haystack, needle) {
        let e = at + needle.len();
        if at >= end && spans.iter().all(|&(s, t)| e <= s || at >= t) {
            n += 1;
            end = e;
        }
    }
    n
}

/// The text of one page, source and output (white space removed), checked against what was
/// done to the page's paragraphs (`outcomes`, all of the page's).
///
/// Each line of the paragraphs is first placed in the source's text ([`place`]), so that a line
/// kept is counted where it is, not inside another paragraph's (`XHelloY` kept and `Hello`
/// translated); a line that cannot be placed is `unplaced`, which does not pass.
pub fn check_text(source: &str, output: &str, outcomes: &[&Outcome], into: &mut TextCheck) {
    let translated: Vec<&&Outcome> = outcomes
        .iter()
        .filter(|o| o.status == Status::Translate)
        .collect();
    let squeeze = |t: &str| -> String { t.chars().filter(|c| !c.is_whitespace()).collect() };
    let translations: Vec<String> = translated
        .iter()
        .map(|o| squeeze(o.translation.as_deref().unwrap_or("")))
        .collect();
    let inserted: String = translations.concat();
    // The times a line should have gone with the sources removed.
    // (In each paragraph's lines together, as the page's text has them: a word may run on from
    // one line to the next.)
    let sources: Vec<String> = translated
        .iter()
        .map(|o| o.paragraph.source_lines.concat())
        .collect();
    let removed = |line: &str| -> usize { sources.iter().map(|m| count(m, line)).sum() };
    let placed = place(source, outcomes);
    // Where the lines of the paragraphs translated were: what is read there in the source is
    // gone, and what is read clear of it should still be read (`86` + `2025年度` holds `62`,
    // which goes with `2025年度` translated).
    let gone: Vec<(usize, usize)> = outcomes
        .iter()
        .zip(&placed)
        .filter(|(o, _)| o.status == Status::Translate)
        .flat_map(|(_, spans)| spans.iter().flatten().copied())
        .collect();
    // A translation is to be read as many times as the translations hold it (two paragraphs
    // translated alike are two), and as the text kept does.
    let wanted = |t: &str, folded: bool| -> (usize, usize) {
        let f = |s: &str| if folded { fold(s) } else { s.to_owned() };
        let t = f(t);
        let need = translations.iter().map(|u| count(&f(u), &t)).sum::<usize>()
            + count_clear(&f(source), &t, &gone);
        (count(&f(output), &t), need)
    };
    for (o, spans) in outcomes.iter().zip(&placed) {
        let id = || o.paragraph.id.clone();
        let lines: Vec<&String> = o
            .paragraph
            .source_lines
            .iter()
            .filter(|l| !l.is_empty())
            .collect();
        let unfound = lines.iter().any(|line| count(source, line) == 0);
        let unplaced = !unfound
            && o.paragraph
                .source_lines
                .iter()
                .zip(spans)
                .any(|(l, s)| !l.is_empty() && s.is_none());
        if unplaced {
            into.unplaced.push(id());
        }
        if o.status == Status::Translate {
            let t = squeeze(o.translation.as_deref().unwrap_or(""));
            let (got, need) = wanted(&t, false);
            if got < need {
                into.missing.push(id());
                let (got, need) = wanted(&t, true);
                if got >= need {
                    into.substituted.push(id());
                }
            }
            if unfound {
                into.unverified.push(id());
            }
            // No more times in the output than in the source, less those removed, but for the
            // translations' own.
            let gone = lines.iter().all(|line| {
                let before = count(source, line);
                before == 0
                    || count(output, line) + removed(line) <= before + count(&inserted, line)
            });
            if !gone {
                into.remaining.push(id());
            }
        } else {
            if unfound {
                into.kept_unverified.push(id());
            }
            // As many times in the output as the source has it clear of the lines removed (its
            // own place among them), not counting the translations'.
            let kept = lines.iter().all(|line| {
                count(output, line) >= count_clear(source, line, &gone) + count(&inserted, line)
            });
            if !kept {
                into.lost.push(id());
            }
        }
    }
}

/// The boxes, in pixels at `scale`, outside which a page should not change: of the paragraphs
/// translated, their source's and their translation's, with 1.5 pt around.
pub fn translated_boxes(outcomes: &[&Outcome], scale: f64) -> Vec<Rect> {
    let pad = 1.5;
    outcomes
        .iter()
        .filter(|o| o.status == Status::Translate)
        .map(|o| {
            let r = o.paragraph.rect;
            let d = o.drawn.unwrap_or(r);
            Rect {
                left: (r.left.min(d.left) - pad) * scale,
                right: (r.right.max(d.right) + pad) * scale,
                top: (r.top.min(d.top) - pad) * scale,
                bottom: (r.bottom.max(d.bottom) + pad) * scale,
            }
        })
        .collect()
}

/// The shares of the pixels (of `w` × `h`, `channels` bytes each, the first three compared)
/// that differ by more than 24 in a color between `a` and `b`: outside `boxes` and inside.
pub fn changed_pixels(
    a: &[u8],
    b: &[u8],
    w: usize,
    h: usize,
    channels: usize,
    boxes: &[Rect],
) -> (f64, f64) {
    let (outside, inside, _) = pixel_changes(a, b, w, h, channels, boxes);
    (outside, inside)
}

/// The pixels outside `boxes` that `b` has lighter than `a` by more than 24 in a color: ink
/// lost (of `w` × `h`, `channels` bytes each, the first three compared).
pub fn lost_ink(a: &[u8], b: &[u8], w: usize, h: usize, channels: usize, boxes: &[Rect]) -> usize {
    let mut lost = 0;
    for y in 0..h {
        for x in 0..w {
            let k = (y * w + x) * channels;
            if (0..3).any(|c| b[k + c] > a[k + c].saturating_add(24)) {
                let (fx, fy) = (x as f64 + 0.5, y as f64 + 0.5);
                if !boxes
                    .iter()
                    .any(|r| fx >= r.left && fx <= r.right && fy >= r.top && fy <= r.bottom)
                {
                    lost += 1;
                }
            }
        }
    }
    lost
}

/// Pixel differences (>24 levels) and any darkened outside pixels (no noise allowance).
pub fn pixel_changes(
    a: &[u8],
    b: &[u8],
    w: usize,
    h: usize,
    channels: usize,
    boxes: &[Rect],
) -> (f64, f64, usize) {
    let (mut outside, mut inside, mut darkened) = (0usize, 0usize, 0usize);
    for y in 0..h {
        for x in 0..w {
            let k = (y * w + x) * channels;
            let diff = (0..3)
                .map(|c| a[k + c].abs_diff(b[k + c]))
                .max()
                .unwrap_or(0);
            let (fx, fy) = (x as f64 + 0.5, y as f64 + 0.5);
            let masked = boxes
                .iter()
                .any(|r| fx >= r.left && fx <= r.right && fy >= r.top && fy <= r.bottom);
            if !masked && (0..3).any(|c| b[k + c] < a[k + c]) {
                darkened += 1;
            }
            if diff > 24 {
                if masked {
                    inside += 1;
                } else {
                    outside += 1;
                }
            }
        }
    }
    let all = (w * h).max(1) as f64;
    (outside as f64 / all, inside as f64 / all, darkened)
}

/// Checks a write's output against its source: page count, the paragraphs written those of the
/// source (analyzed again, over the same `pages`), the translations in its text, their sources
/// gone from it, the paragraphs kept still in it, no translation over another paragraph, and
/// the pixels outside the paragraphs translated unchanged. The text and the pixels are PDFium's
/// only, on the pages worked on.
pub fn check(
    original: &[u8],
    output: &[u8],
    outcomes: &[Outcome],
    pages: Option<usize>,
    scale: f64,
) -> Result<Check, Error> {
    let analysis = analyze(original, pages)?;
    let mut report = Check::default();
    let mut ids: HashSet<&str> = HashSet::new();
    for info in &analysis.analyzed {
        for p in &info.paragraphs {
            ids.insert(p.id.as_str());
        }
    }
    report.extracted = ids.len();
    let mut seen: HashSet<&str> = HashSet::new();
    for o in outcomes {
        let id = o.paragraph.id.as_str();
        if !ids.contains(id) || !seen.insert(id) {
            report.unknown.push(id.to_owned());
        }
        match o.status {
            Status::Translate => report.translated += 1,
            Status::Retain(_) => report.retained += 1,
            Status::Skip(_) => report.skipped += 1,
        }
    }
    let mut unaccounted: Vec<String> = ids
        .iter()
        .filter(|id| !seen.contains(*id))
        .map(|id| (*id).to_owned())
        .collect();
    unaccounted.sort();
    report.unaccounted = unaccounted;
    let (_guard, pdfium) = pdfium()?;
    let lib = pdfium.bindings();
    let a = Doc::load(lib, original)?;
    let b = Doc::load(lib, output)?;
    let erased_copy = Doc::load(lib, original)?;
    report.pages_original = a.pages();
    report.pages_output = b.pages();
    for i in 0..analysis.analyzed.len().min(b.pages()) {
        let pa = a.page(i)?;
        let pb = b.page(i)?;
        let here: Vec<&Outcome> = outcomes.iter().filter(|o| o.paragraph.page == i).collect();
        check_text(
            &page_text(lib, &pa),
            &page_text(lib, &pb),
            &here,
            &mut report.text,
        );
        report.overlaps.extend(overlaps(&here));
        let erased_page = erased_copy.page(i)?;
        let found = verified_found(
            lib,
            &erased_page,
            i,
            analysis.body_size,
            analysis.leading_limit,
        );
        // The glyphs erased (PDFium's boxes of their ink), for the ink lost outside.
        let mut erased_glyphs = Vec::new();
        for (j, p) in found.paragraphs.iter().enumerate() {
            if here
                .iter()
                .any(|o| o.paragraph.id == p.id && o.status == Status::Translate)
            {
                erased_glyphs.extend(selected_characters(lib, &erased_page, &found, j, false)?);
            }
        }
        let taken = take_text(lib, &erased_page, &found, |j| {
            here.iter()
                .any(|o| o.paragraph.id == found.paragraphs[j].id && o.status == Status::Translate)
        })?;
        let clean = render(lib, &erased_page, AUDIT_SCALE);
        let painted = render(lib, &pb, AUDIT_SCALE);
        for (_, object) in taken {
            lib.FPDFPageObj_Destroy(object);
        }
        let area = found.scan.frame.width * found.scan.frame.height;
        let obstacles: Vec<_> = analysis.analyzed[i]
            .obstacles
            .iter()
            .copied()
            .filter(|r| (r.right - r.left) * (r.bottom - r.top) <= area * 0.2)
            .collect();
        for o in &here {
            let j = found
                .paragraphs
                .iter()
                .position(|p| p.id == o.paragraph.id)
                .unwrap();
            let own_obstacles: Vec<_> = obstacles
                .iter()
                .copied()
                .filter(|r| {
                    !found
                        .scan
                        .links
                        .iter()
                        .any(|link| link == r && own_link(&found, j, link))
                })
                .collect();
            report.crossings.extend(crossings(&[o], &own_obstacles));
        }
        for o in here
            .iter()
            .filter(|o| o.status == Status::Translate && o.drawn.is_some())
        {
            let color = background(&o.paragraph.rect, &clean);
            let fallback = [o.drawn.unwrap()];
            let lines = if o.line_rects.is_empty() {
                &fallback[..]
            } else {
                &o.line_rects
            };
            for (k, r) in lines.iter().enumerate() {
                let id = format!("{}:line{}", o.paragraph.id, k + 1);
                if !clear_pixels(r, color, &clean) {
                    report.occupied_lines.push(id.clone());
                }
                if clean.0 != painted.0
                    || clean.1 != painted.1
                    || !pixels_in(r, &clean)
                        .zip(pixels_in(r, &painted))
                        .any(|(a, b)| (0..3).any(|c| a[c].abs_diff(b[c]) > 12))
                {
                    report.invisible_lines.push(id);
                }
            }
        }
        let (w, h, ra) = render(lib, &pa, scale);
        let (w2, h2, rb) = render(lib, &pb, scale);
        let mut check = PageCheck {
            page: i,
            paths_changed: paths_changed(lib, &pa, &pb),
            ..PageCheck::default()
        };
        if w == w2 && h == h2 && w > 0 {
            let boxes = translated_boxes(&here, scale);
            (check.changed_outside, check.changed_inside) =
                changed_pixels(&ra, &rb, w, h, 4, &boxes);
            let mut kept = boxes;
            kept.extend(erase_boxes(&erased_glyphs, i, scale));
            check.lightened_outside = lost_ink(&ra, &rb, w, h, 4, &kept);
        } else {
            check.changed_outside = 1.0;
            check.lightened_outside = w.max(w2) * h.max(h2);
        }
        report.pages.push(check);
    }
    Ok(report)
}

/// A path as PDFium reads it from a page: how it paints (fill mode, stroke, their colors, the
/// stroke's width, dashes, caps and joins, whether it is drawn with any transparency: a blend,
/// a soft mask, a transparent color), what clips it (each clipping path's points, as PDFium
/// reads them), and its outline, point by point in page space (to a hundredth of a point), with
/// whether each closes.
#[derive(Clone, Debug, PartialEq, Eq, Hash)]
struct PathSig {
    paint: [c_int; 4],
    colors: [[c_uint; 4]; 2],
    width: i64,
    dash: Vec<i64>,
    transparent: bool,
    clip: Vec<i64>,
    points: Vec<(c_int, i64, i64, bool)>,
}

/// The paths of `object` (a page object, or a Form's, `outer` taking a Form's space to the
/// page's), in the order they are drawn, into `out`. `deep` is set where a Form is nested 16 deep
/// or more: what is in it is not read.
fn path_sigs(
    lib: &dyn PdfiumLibraryBindings,
    object: FPDF_PAGEOBJECT,
    outer: &Matrix,
    depth: usize,
    out: &mut Vec<PathSig>,
    deep: &mut bool,
) {
    let kind = lib.FPDFPageObj_GetType(object);
    let m = then(&matrix_of(lib, object), outer);
    if kind == PAGEOBJ_FORM {
        if depth >= 16 {
            *deep = true;
            return;
        }
        for k in 0..lib.FPDFFormObj_CountObjects(object).max(0) {
            let child = lib.FPDFFormObj_GetObject(object, k as c_ulong);
            if !child.is_null() {
                path_sigs(lib, child, &m, depth + 1, out, deep);
            }
        }
        return;
    }
    if kind != PAGEOBJ_PATH {
        return;
    }
    let round = |v: f64| (v * 100.0).round() as i64;
    let (mut fill, mut stroke) = (0, 0);
    lib.FPDFPath_GetDrawMode(object, &mut fill, &mut stroke);
    let mut colors = [[0; 4]; 2];
    let [f, st] = &mut colors;
    lib.FPDFPageObj_GetFillColor(object, &mut f[0], &mut f[1], &mut f[2], &mut f[3]);
    lib.FPDFPageObj_GetStrokeColor(object, &mut st[0], &mut st[1], &mut st[2], &mut st[3]);
    let mut width = 0.0f32;
    lib.FPDFPageObj_GetStrokeWidth(object, &mut width);
    let width = f64::from(width) * (m[0] * m[3] - m[1] * m[2]).abs().sqrt();
    let count = lib.FPDFPageObj_GetDashCount(object).max(0) as usize;
    let mut dash = vec![0.0f32; count];
    if count > 0 {
        lib.FPDFPageObj_GetDashArray(object, dash.as_mut_ptr(), count);
    }
    let mut phase = 0.0f32;
    lib.FPDFPageObj_GetDashPhase(object, &mut phase);
    let mut dash: Vec<i64> = dash.iter().map(|d| round(f64::from(*d))).collect();
    dash.push(round(f64::from(phase)));
    // No clip: nothing; a clip of text only: its mark and no path.
    let mut clip = Vec::new();
    let handle = lib.FPDFPageObj_GetClipPath(object);
    if !handle.is_null() {
        clip.push(-2);
        for p in 0..lib.FPDFClipPath_CountPaths(handle).max(0) {
            clip.push(-1);
            for k in 0..lib.FPDFClipPath_CountPathSegments(handle, p).max(0) {
                let seg = lib.FPDFClipPath_GetPathSegment(handle, p, k);
                let (mut x, mut y) = (0.0f32, 0.0f32);
                if seg.is_null() || lib.FPDFPathSegment_GetPoint(seg, &mut x, &mut y) == 0 {
                    clip.push(i64::MIN);
                    continue;
                }
                clip.extend([
                    i64::from(lib.FPDFPathSegment_GetType(seg)),
                    round(f64::from(x)),
                    round(f64::from(y)),
                    i64::from(lib.FPDFPathSegment_GetClose(seg) != 0),
                ]);
            }
        }
    }
    let mut points = Vec::new();
    for k in 0..lib.FPDFPath_CountSegments(object).max(0) {
        let seg = lib.FPDFPath_GetPathSegment(object, k);
        let (mut x, mut y) = (0.0f32, 0.0f32);
        if seg.is_null() || lib.FPDFPathSegment_GetPoint(seg, &mut x, &mut y) == 0 {
            points.push((-1, 0, 0, false));
            continue;
        }
        let (x, y) = apply(&m, f64::from(x), f64::from(y));
        points.push((
            lib.FPDFPathSegment_GetType(seg),
            round(x),
            round(y),
            lib.FPDFPathSegment_GetClose(seg) != 0,
        ));
    }
    out.push(PathSig {
        paint: [
            fill,
            stroke,
            lib.FPDFPageObj_GetLineCap(object),
            lib.FPDFPageObj_GetLineJoin(object),
        ],
        colors,
        width: round(width),
        dash,
        transparent: lib.FPDFPageObj_HasTransparency(object) != 0,
        clip,
        points,
    });
}

/// How many paths `after` does not draw as `before` did, with none let pass: every path of the
/// page (on it and in its Forms) is to be drawn again, point for point, in the same paint and
/// clip and in the same order, and no other is to be drawn (a translation is text only, and no
/// path is ever taken off a page). Each path `before` draws more times than `after` counts one,
/// and each `after` draws more times than `before`; the same paths in another order count one;
/// a page with a Form nested 16 deep or more, which cannot be compared, counts one. What a path
/// is is read from the content alone: the writer is not taken on trust.
fn paths_changed(lib: &dyn PdfiumLibraryBindings, before: &Page<'_>, after: &Page<'_>) -> usize {
    let mut deep = false;
    let mut read = |page: &Page<'_>| {
        let mut out = Vec::new();
        for k in 0..lib.FPDFPage_CountObjects(page.raw) {
            let object = lib.FPDFPage_GetObject(page.raw, k);
            if !object.is_null() {
                path_sigs(lib, object, &IDENTITY, 0, &mut out, &mut deep);
            }
        }
        out
    };
    let (a, b) = (read(before), read(after));
    fn count(list: &[PathSig]) -> HashMap<&PathSig, usize> {
        let mut by = HashMap::new();
        for sig in list {
            *by.entry(sig).or_default() += 1;
        }
        by
    }
    let (in_a, in_b) = (count(&a), count(&b));
    let more = |x: &HashMap<&PathSig, usize>, y: &HashMap<&PathSig, usize>| {
        x.iter()
            .map(|(sig, n)| n.saturating_sub(y.get(sig).copied().unwrap_or(0)))
            .sum::<usize>()
    };
    let mut changed = more(&in_a, &in_b) + more(&in_b, &in_a);
    if changed == 0 && a != b {
        changed += 1;
    }
    if deep {
        changed += 1;
    }
    changed
}

#[cfg(all(test, feature = "pdf-erase"))]
mod underlines;

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn only_lighter_pixels_outside_the_boxes_are_lost_ink() {
        let (w, h) = (4, 2);
        let a = vec![0u8; w * h * 4];
        let mut b = a.clone();
        b[..3].fill(200); // (0, 0): ink gone
        b[4..7].fill(20); // (1, 0): within the noise
        b[(w + 3) * 4..(w + 3) * 4 + 3].fill(255); // (3, 1): ink gone, in a box
        let boxes = [Rect {
            left: 3.0,
            top: 1.0,
            right: 4.0,
            bottom: 2.0,
        }];
        assert_eq!(lost_ink(&a, &b, w, h, 4, &boxes), 1);
        // Darker is not lost ink.
        assert_eq!(lost_ink(&b, &a, w, h, 4, &[]), 0);
    }

    #[test]
    fn a_hanging_reference_continuation_shares_scale_across_link_colors() {
        let a = Rect {
            left: 72.0,
            right: 290.0,
            top: 100.0,
            bottom: 109.0,
        };
        let b = Rect {
            left: 83.0,
            right: 290.0,
            top: 111.0,
            bottom: 145.0,
        };
        assert!(hanging_continuation(&a, &b, 10.0));
        assert!(!hanging_continuation(&a, &Rect { left: 307.0, ..b }, 10.0));
        assert!(!hanging_continuation(&a, &Rect { top: 120.0, ..b }, 10.0));
    }

    #[test]
    fn column_edge_uses_its_own_paragraphs_not_the_gutter_or_footer() {
        let r = |left, right, top| Rect {
            left,
            right,
            top,
            bottom: top + 12.0,
        };
        let source = r(54.0, 250.0, 50.0);
        let paragraphs = [
            source,
            r(54.0, 290.0, 80.0),
            r(54.0, 291.0, 100.0),
            r(54.0, 292.0, 120.0),
            r(309.0, 550.0, 50.0),
            r(300.0, 305.0, 750.0),
        ];
        assert_eq!(column_right(&source, 10.0, 612.0, &paragraphs), 291.0);
        let full = [
            source,
            r(54.0, 550.0, 80.0),
            r(54.0, 550.0, 100.0),
            r(300.0, 305.0, 750.0),
        ];
        assert_eq!(column_right(&source, 10.0, 612.0, &full), 576.0);
        let label = r(100.0, 155.0, 150.0);
        assert_eq!(
            column_right(&label, 7.0, 612.0, &[label, r(210.0, 310.0, 150.0)]),
            203.0
        );
        let centered = r(270.0, 320.0, 150.0);
        assert_eq!(
            column_right(&centered, 12.0, 612.0, &[centered, r(400.0, 520.0, 30.0)]),
            576.0
        );
    }

    #[test]
    fn darkened_pixels_have_no_tolerance_outside_the_mask() {
        assert_eq!(
            pixel_changes(&[255; 3], &[254; 3], 1, 1, 3, &[]),
            (0.0, 0.0, 1)
        );
        assert_eq!(
            pixel_changes(&[0; 3], &[255; 3], 1, 1, 3, &[]),
            (1.0, 0.0, 0)
        );
        assert_eq!(
            pixel_changes(
                &[255; 3],
                &[0; 3],
                1,
                1,
                3,
                &[Rect {
                    left: 0.0,
                    right: 1.0,
                    top: 0.0,
                    bottom: 1.0
                }]
            ),
            (0.0, 1.0, 0)
        );
    }

    fn fit_frame() -> Frame2 {
        Frame2 {
            left: 0.0,
            right: 100.0,
            top: 0.0,
            limit: 20.0,
            narrow_limit: 20.0,
            size: 10.0,
            ascent: 0.8,
            descent: 0.2,
            line_height: 2.0,
            align: Align::Left,
            one_line: false,
            single_only: false,
        }
    }

    #[test]
    #[ignore = "requires PRIVATE_TRANSLATE_PDF_FONT"]
    fn centered_expansion_shrink_and_insets_keep_the_source_center() {
        let bytes = output_font().unwrap();
        let face = Face::parse(&bytes).unwrap();
        let found = found(&[("A centered title", 240.0, 340.0, 100.0, 12.0)], vec![]);
        assert_eq!(found.paragraphs[0].align, Align::Center);
        let original = frame_of(&found.blocks[0], &found.paragraphs[0], 115.0);
        let expanded = expanded_frame(&found, 0, &original);
        assert!((expanded.left + expanded.right - 580.0).abs() < 1e-8);
        for max_scale in [1.0, 0.75] {
            let (setting, _) = fit(
                &atoms("A somewhat longer translated title"),
                &face,
                &original,
                &expanded,
                1.0,
                0.5,
                |s| s.scale <= max_scale && s.drawn.top >= original.top + 0.5,
            );
            let setting = setting.unwrap();
            for (line, x, _) in &setting.lines {
                assert!((x + face.width(line) * setting.size / 2.0 - 290.0).abs() < 1e-8);
            }
        }
    }

    #[test]
    #[ignore = "requires PRIVATE_TRANSLATE_PDF_FONT"]
    fn short_single_lines_wrap_before_shrinking_below_eighty_five_percent() {
        let bytes = output_font().unwrap();
        let face = Face::parse(&bytes).unwrap();
        let mut f = fit_frame();
        f.one_line = true;
        f.right = face.width("word word") * f.size + 0.5;
        f.limit = 100.0;
        let (setting, attempts) = fit(&atoms("word word word"), &face, &f, &f, 1.0, 0.5, |_| true);
        let setting = setting.unwrap();
        assert_eq!(setting.scale, 1.0);
        assert_eq!(setting.lines.len(), 2);
        assert!(attempts.iter().all(|a| a.scale >= 0.85 - 1e-8));
    }

    #[test]
    #[ignore = "requires PRIVATE_TRANSLATE_PDF_FONT"]
    fn multi_line_text_uses_empty_space_above_before_shrinking() {
        let bytes = output_font().unwrap();
        let face = Face::parse(&bytes).unwrap();
        let mut f = fit_frame();
        f.top = 100.0;
        f.limit = 200.0;
        f.right = face.width("word word") * f.size + 0.5;
        let text = atoms("word word word word");
        let height = set_at(&text, &face, &f, 1.0, false).unwrap().drawn.bottom;
        f.limit = height - 2.0;
        let (s, _) = fit(&text, &face, &f, &f, 1.0, 0.5, |s| s.drawn.top >= 98.0);
        let s = s.unwrap();
        assert_eq!(s.scale, 1.0);
        assert!(s.drawn.top < 100.0 && s.drawn.top >= 98.0);
        let (s, _) = fit(&text, &face, &f, &f, 1.0, 0.5, |s| s.drawn.top >= 100.0);
        assert!(
            s.unwrap().drawn.top >= 100.0,
            "the upper obstacle is still mandatory"
        );
    }

    #[test]
    fn body_lines_at_a_column_edge_are_not_centered() {
        let f = found(
            &[("Full width body line", 72.0, 520.0, 100.0, 12.0)],
            vec![],
        );
        assert_eq!(f.paragraphs[0].align, Align::Left);
    }

    #[test]
    fn erased_pixels_accept_flat_color_and_noise_but_reject_content() {
        let r = Rect {
            left: 0.0,
            right: 4.0,
            top: 0.0,
            bottom: 4.0,
        };
        let mut bitmap = (8, 8, vec![200; 8 * 8 * 4]);
        assert!(clear_pixels(&r, background(&r, &bitmap), &bitmap));
        bitmap.2[32] = 198;
        assert!(clear_pixels(&r, background(&r, &bitmap), &bitmap));
        bitmap.2[32] = 100;
        assert!(!clear_pixels(&r, background(&r, &bitmap), &bitmap));
    }

    #[test]
    #[ignore = "requires PRIVATE_TRANSLATE_PDF_FONT"]
    fn a_small_inset_avoids_contact_without_reducing_font_size() {
        let bytes = output_font().unwrap();
        let face = Face::parse(&bytes).unwrap();
        let mut f = fit_frame();
        f.limit = 100.0;
        let (s, _) = fit(&atoms("Text"), &face, &f, &f, 1.0, 0.5, |s| {
            s.drawn.top >= f.top + 0.5 && s.drawn.left >= f.left
        });
        let s = s.unwrap();
        assert_eq!(s.scale, 1.0);
        assert!(s.drawn.top >= 0.5 && s.drawn.left >= 0.0);
    }

    #[test]
    #[ignore = "requires PRIVATE_TRANSLATE_PDF_FONT"]
    fn line_rectangles_enclose_output_glyph_extrema_with_half_point_padding() {
        let bytes = output_font().unwrap();
        let face = Face::parse(&bytes).unwrap();
        let mut frame = fit_frame();
        frame.limit = 100.0;
        frame.right = 300.0;
        let s = set_at(&atoms("日本語 gj Á"), &face, &frame, 1.0, false).unwrap();
        for ((text, _, y), r) in s.lines.iter().zip(&s.line_rects) {
            for c in text.chars() {
                if let Some(b) = translation_glyph(&face.fonts.fonts[0], c)
                    .and_then(|g| face.fonts.fonts[0].glyph_bounding_box(g))
                {
                    assert!(r.top <= y - f64::from(b.y_max) / face.upem * s.size - 0.5 + 1e-8);
                    assert!(r.bottom >= y - f64::from(b.y_min) / face.upem * s.size + 0.5 - 1e-8);
                }
            }
        }
    }

    #[test]
    fn fitting_floors_keep_small_type_at_four_points() {
        let mut f = fit_frame();
        for (size, expected) in [(12.0, 0.5), (8.0, 0.5), (6.0, 4.0 / 6.0), (3.0, 4.0 / 3.0)] {
            f.size = size;
            assert_eq!(floor(&f, 0.5), expected);
        }
        assert_eq!(leading_floor(&atoms("English words")), 1.15);
        assert_eq!(leading_floor(&atoms("日本語の訳文")), 1.3);
    }

    #[test]
    fn expansion_cannot_cross_a_rule_or_enlarge_an_existing_intersection() {
        let source = rect(10.0, 20.0);
        let drawn = rect(10.0, 40.0);
        assert!(!clear_expansion(&source, &drawn, &[rect(30.0, 30.0)]));
        assert!(!clear_expansion(&source, &drawn, &[rect(15.0, 50.0)]));
        assert!(clear_expansion(&source, &drawn, &[rect(50.0, 60.0)]));
    }

    #[test]
    #[ignore = "requires PRIVATE_TRANSLATE_PDF_FONT"]
    fn fitting_operations_are_ordered_and_record_the_final_leading() {
        let bytes = output_font().unwrap();
        let face = Face::parse(&bytes).unwrap();
        let text = atoms("word word word word");
        let mut f = fit_frame();
        // Reserve the raster margin so this fixture exercises the vertical
        // fitting sequence rather than forcing a horizontal shrink.
        f.right = face.width("word word") * f.size + 0.75;
        let ink_height = (f
            .ascent
            .max(f64::from(face.fonts.fonts[0].ascender()) / face.upem)
            + f.descent
                .max(-f64::from(face.fonts.fonts[0].descender()) / face.upem))
            * f.size
            + 1.0;
        f.limit = ink_height + 20.0;
        let (s, a) = fit(&text, &face, &f, &f, 1.0, 0.5, |_| true);
        assert_eq!(s.unwrap().stage, FitStage::Original);
        assert_eq!(a.len(), 1);
        f.limit = ink_height + 12.0;
        let (s, _) = fit(&text, &face, &f, &f, 1.0, 0.5, |_| true);
        let s = s.unwrap();
        assert_eq!(s.stage, FitStage::Leading);
        assert!(s.line_height <= 12.5);
        f.limit = 10.0;
        let mut expanded = f.clone();
        expanded.limit = ink_height + 14.0;
        let (s, _) = fit(&text, &face, &f, &expanded, 1.0, 0.5, |_| true);
        assert_eq!(s.unwrap().stage, FitStage::Expand);
        f.limit = 12.0;
        let (s, attempts) = fit(&text, &face, &f, &f, 1.0, 0.5, |_| true);
        let s = s.unwrap();
        assert_eq!(s.stage, FitStage::Shrink);
        assert!((s.scale * 20.0 - (s.scale * 20.0).round()).abs() < 1e-9);
        assert!(attempts.iter().any(|a| a.stage == FitStage::Leading));
        let mut wider = f.clone();
        wider.right *= 2.0;
        let (s, _) = fit(&text, &face, &f, &wider, 1.0, 0.5, |s| {
            s.drawn.right <= f.right
        });
        assert_eq!(s.unwrap().stage, FitStage::Shrink);
        // Retrying the original width has its own taller free area.
        let mut wider = f.clone();
        wider.right *= 2.0;
        wider.limit = 10.0;
        wider.narrow_limit = ink_height + 20.0;
        let (s, _) = fit(&text, &face, &f, &wider, 1.0, 0.5, |s| {
            s.drawn.right <= f.right
        });
        assert_eq!(s.unwrap().scale, 1.0);
        let mut single = f.clone();
        single.one_line = true;
        let (_, attempts) = fit(&text, &face, &single, &single, 1.0, 0.5, |_| true);
        assert!(attempts.iter().all(|a| a.stage != FitStage::Leading));
        f.limit = 1.0;
        let (s, _) = fit(&text, &face, &f, &f, 1.0, 0.5, |_| true);
        assert_eq!(s.unwrap().stage, FitStage::Overflow);
        let (s, a) = fit(&text, &face, &f, &f, 1.0, 0.5, |_| false);
        assert!(s.is_none());
        assert_eq!(a.last().unwrap().stage, FitStage::Overflow);
        assert_eq!(a.last().unwrap().scale, 0.5);
    }

    #[cfg(feature = "pdf-erase")]
    fn decorate_sample(prefix: &str, suffix: &str, alpha: Option<f32>) -> Vec<u8> {
        use lopdf::{Object, dictionary};
        let mut doc = lopdf::Document::load_mem(&sample()).unwrap();
        let page = doc.get_pages()[&1];
        let mut content = doc.get_page_content(page);
        if prefix.trim() == "1 Tr" {
            content = String::from_utf8(content)
                .unwrap()
                .replace("0 Tr", "1 Tr")
                .into_bytes();
        }
        if alpha.is_some() {
            content = String::from_utf8(content)
                .unwrap()
                .replace(" Tf", " Tf /Invisible gs")
                .into_bytes();
        }
        if let Some(alpha) = alpha {
            let resources = doc
                .get_dictionary(page)
                .unwrap()
                .get(b"Resources")
                .unwrap()
                .clone();
            let mut resources = doc
                .dereference(&resources)
                .unwrap()
                .1
                .as_dict()
                .unwrap()
                .clone();
            resources.set(
                "ExtGState",
                dictionary! { "Invisible" => dictionary! { "ca" => Object::Real(alpha) } },
            );
            doc.get_object_mut(page)
                .unwrap()
                .as_dict_mut()
                .unwrap()
                .set("Resources", resources);
        }
        let mut out = prefix.as_bytes().to_vec();
        out.extend(content);
        out.extend(suffix.as_bytes());
        let stream = doc.add_object(lopdf::Stream::new(lopdf::Dictionary::new(), out));
        doc.get_object_mut(page)
            .unwrap()
            .as_dict_mut()
            .unwrap()
            .set("Contents", stream);
        let mut bytes = Vec::new();
        doc.save_to(&mut bytes).unwrap();
        bytes
    }

    /// `bytes` with a link annotation (to a URL) over each page-space `[left, bottom, right, top]`
    /// of `rects` on its first page.
    #[cfg(feature = "pdf-erase")]
    fn with_links(bytes: &[u8], rects: &[[f64; 4]]) -> Vec<u8> {
        use lopdf::{Object, dictionary};
        let mut doc = lopdf::Document::load_mem(bytes).unwrap();
        let page = doc.get_pages()[&1];
        let mut annots: Vec<Object> = doc
            .get_dictionary(page)
            .unwrap()
            .get(b"Annots")
            .ok()
            .and_then(|a| doc.dereference(a).ok())
            .and_then(|(_, a)| a.as_array().ok().cloned())
            .unwrap_or_default();
        for r in rects {
            let annot = doc.add_object(dictionary! {
                "Type" => "Annot",
                "Subtype" => "Link",
                "Rect" => r.iter().map(|v| Object::Real(*v as f32)).collect::<Vec<_>>(),
                "Border" => vec![0.into(), 0.into(), 0.into()],
                "A" => dictionary! { "S" => "URI", "URI" => Object::string_literal("https://example.invalid/") },
            });
            annots.push(annot.into());
        }
        doc.get_object_mut(page)
            .unwrap()
            .as_dict_mut()
            .unwrap()
            .set("Annots", annots);
        let mut out = Vec::new();
        doc.save_to(&mut out).unwrap();
        out
    }

    #[test]
    #[cfg(feature = "pdf-erase")]
    #[ignore = "requires local PDFium and IPA font"]
    fn transparent_search_text_is_preserved_and_missing_paint_fails_the_audit() {
        for alpha in [0.0, 0.005] {
            let source = decorate_sample("/Invisible gs\n", "", Some(alpha));
            let a = analyze(&source, None).unwrap();
            assert!(
                a.analyzed[0]
                    .paragraphs
                    .iter()
                    .all(|p| p.status == Status::Skip(Skip::Invisible))
            );
            let (_, plan) = write(
                &source,
                None,
                &HashMap::from([("p1-0".into(), "Visible translation".into())]),
                Options::default(),
            )
            .unwrap();
            assert!(plan.iter().all(|p| p.status != Status::Translate));
        }
        let source = sample();
        let translations = HashMap::from([("p1-0".into(), "Visible translation".into())]);
        let (output, plan) = write(&source, None, &translations, Options::default()).unwrap();
        let good = check(&source, &output, &plan, None, 1.25).unwrap();
        assert!(good.invisible_lines.is_empty());
        let (erased, _) = erase_pdfium(&source, None, &translations, Options::default()).unwrap();
        assert!(
            !check(&source, &erased, &plan, None, 1.25)
                .unwrap()
                .invisible_lines
                .is_empty()
        );
    }

    #[test]
    #[cfg(feature = "pdf-erase")]
    #[ignore = "requires local PDFium and IPA font"]
    fn erased_page_rules_inside_source_boxes_block_unsafe_settings() {
        let plain = sample();
        let translations = HashMap::from([(
            "p1-1".into(),
            "A translation that needs several lines in the original paragraph box.".repeat(2),
        )]);
        let (_, plan) = write(&plain, None, &translations, Options::default()).unwrap();
        let line = plan[1].line_rects[0];
        let height = analyze(&plain, None).unwrap().analyzed[0].height;
        let y = height - (line.top + line.bottom) / 2.0;
        let source = decorate_sample("", &format!("\n0 G 1 w 70 {y} m 520 {y} l S\n"), None);
        let (output, plan) = write(&source, None, &translations, Options::default()).unwrap();
        let checked = check(&source, &output, &plan, None, 1.25).unwrap();
        assert!(
            checked.occupied_lines.is_empty(),
            "{:?}",
            checked.occupied_lines
        );
        assert!(plan[1].status != Status::Translate || plan[1].line_rects[0] != line);
    }

    /// How many paths of the first page of `out` are not drawn as the first page of `source`
    /// draws them ([`paths_changed`], with none let pass).
    #[cfg(feature = "pdf-erase")]
    fn paths_apart(source: &[u8], out: &[u8]) -> usize {
        let (_guard, pdfium) = pdfium().unwrap();
        let lib = pdfium.bindings();
        let (a, b) = (
            Doc::load(lib, source).unwrap(),
            Doc::load(lib, out).unwrap(),
        );
        paths_changed(lib, &a.page(0).unwrap(), &b.page(0).unwrap())
    }

    /// The thin rules of the first page of `bytes` (to a hundredth of a point), in order.
    #[cfg(feature = "pdf-erase")]
    fn rules_of(bytes: &[u8]) -> Vec<[i64; 4]> {
        let (_guard, pdfium) = pdfium().unwrap();
        let doc = read_doc(&pdfium, bytes).unwrap();
        let mut r: Vec<[i64; 4]> = scan(doc.lib, &doc.page(0).unwrap())
            .decorations
            .iter()
            .map(|(_, r)| [r.left, r.top, r.right, r.bottom].map(|v| (v * 100.0).round() as i64))
            .collect();
        r.sort_unstable();
        r
    }

    #[test]
    #[cfg(feature = "pdf-erase")]
    #[ignore = "requires local PDFium and IPA font"]
    fn a_paragraph_underlined_under_its_links_stays_as_drawn() {
        let plain = sample();
        let (lines, height) = {
            let (_guard, pdfium) = pdfium().unwrap();
            let doc = read_doc(&pdfium, &plain).unwrap();
            let f = find(scan(doc.lib, &doc.page(0).unwrap()), 0, 10.0);
            let lines: Vec<(f64, f64, f64)> = f.blocks[1]
                .lines
                .iter()
                .map(|l| (l.left, l.right, l.y))
                .collect();
            (lines, f.scan.frame.height)
        };
        assert_eq!(lines.len(), 2);
        // The paragraph a link from end to end of both its lines, as Word draws one: under each
        // line its underline, drawn after the first line; over each line an annotation. A rule
        // across the page's foot, after everything.
        let rules: String = lines
            .iter()
            .map(|(x, right, baseline)| {
                format!(
                    "0 g {} {} {} 0.6 re f\n",
                    x + 1.0,
                    height - baseline - 4.0,
                    right - x - 2.0
                )
            })
            .collect();
        let unlinked = sample_after_text(2, &rules, "0 G 0.5 w 60 100 m 540 100 l S");
        let link = |(x, right, baseline): (f64, f64, f64)| {
            [
                x - 1.25,
                height - baseline - 5.0,
                right + 1.2,
                height - baseline + 8.0,
            ]
        };
        let translations = HashMap::from([("p1-1".into(), "A translated paragraph.".into())]);
        // With the links over both lines, over the first only, or none: the underlines stay,
        // and their paragraph with them (none of its text is erased); every path is drawn as
        // it was, and the other paragraph's translation is set.
        for source in [
            unlinked.clone(),
            with_links(&unlinked, &[link(lines[0])]),
            with_links(&unlinked, &[link(lines[0]), link(lines[1])]),
        ] {
            let (plan, chars) =
                erase_plan(&source, None, &translations, Options::default()).unwrap();
            assert_eq!(plan[1].status, Status::Retain(Retain::Decoration));
            assert!(chars.iter().all(|c| c.paragraph != "p1-1"));
            let out = super::super::export::write(&source, None, &translations, Options::default())
                .unwrap();
            assert_eq!(out.outcomes[1].status, Status::Retain(Retain::Decoration));
            assert_eq!(paths_apart(&source, &out.bytes), 0);
            assert_eq!(rules_of(&source), rules_of(&out.bytes));
            let checked = check(&source, &out.bytes, &out.outcomes, None, 1.25).unwrap();
            assert!(checked.pages.iter().all(|p| p.paths_changed == 0));
        }
    }

    /// `sample()` with `insert` drawn just after its `n`th text object (from 1), as a
    /// producer draws a line's underline, and `suffix` after everything.
    #[cfg(feature = "pdf-erase")]
    fn sample_after_text(n: usize, insert: &str, suffix: &str) -> Vec<u8> {
        let mut doc = lopdf::Document::load_mem(&sample()).unwrap();
        let page = doc.get_pages()[&1];
        let content = String::from_utf8(doc.get_page_content(page)).unwrap();
        let mut at = 0;
        for _ in 0..n {
            at += content[at..].find("ET").unwrap() + 2;
        }
        let out = format!("{}\n{insert}\n{}\n{suffix}", &content[..at], &content[at..]);
        let stream = doc.add_object(lopdf::Stream::new(
            lopdf::Dictionary::new(),
            out.into_bytes(),
        ));
        doc.get_object_mut(page)
            .unwrap()
            .as_dict_mut()
            .unwrap()
            .set("Contents", stream);
        let mut bytes = Vec::new();
        doc.save_to(&mut bytes).unwrap();
        bytes
    }

    #[test]
    #[cfg(feature = "pdf-erase")]
    #[ignore = "requires local PDFium and IPA font"]
    fn a_heading_underlined_end_to_end_is_set_over_its_rule() {
        let right = 72.0 + 37.0 * 7.2 * 0.95;
        // The heading a link, its annotation as Word puts one (2.25 pt past its ends, down to
        // 1 pt below the rule's foot).
        let linked = |offset: f64, bytes: Vec<u8>| {
            with_links(&bytes, &[[69.75, 740.0 - offset - 1.6, right + 2.2, 750.1]])
        };
        let text = "BT /F1 12 Tf 0 Tr 95 Tz 1 0 0 1 72 740 Tm (Gardening group keeps the beds green.) Tj ET\n";
        // The rule 4 or 4.6 pt below the baseline, clear of the descenders, drawn right after
        // the heading; under its link or not; with a rule standing at its start, 0.5 pt from it
        // (a cell's side); drawn apart from the heading (after a rule across the page's foot).
        let sources = [
            ruled_line(4.0, ""),
            ruled_line(4.6, ""),
            linked(4.0, ruled_line(4.0, "")),
            linked(4.6, ruled_line(4.6, "")),
            linked(
                4.6,
                ruled_line(4.6, "0 G 0.5 w 71.5 731 m 71.5 735.4 l S\n"),
            ),
            linked(
                4.6,
                courier_page(
                    &format!(
                        "{text}0 G 0.5 w 60 100 m 540 100 l S\n0 g 72 735.4 {} 0.6 re f\n",
                        right - 72.0
                    ),
                    &[],
                ),
            ),
        ];
        let translations =
            HashMap::from([("p1-0".into(), "Translated gardening information.".into())]);
        for (k, source) in sources.iter().enumerate() {
            // One line, one rule from end to end of it, and no other: its translation may be
            // set over the rule, which stays as it is drawn, link or no link.
            {
                let (_guard, pdfium) = pdfium().unwrap();
                let doc = read_doc(&pdfium, source).unwrap();
                let (body, leading) = document_body(&doc).unwrap();
                let f = verified_found(doc.lib, &doc.page(0).unwrap(), 0, body, leading);
                assert!(f.kept_underlines.contains(&0), "{k}");
                assert_eq!(f.paragraphs[0].status, Status::Translate, "{k}");
            }
            let out = super::super::export::write(source, None, &translations, Options::default())
                .unwrap();
            assert_eq!(out.outcomes[0].status, Status::Translate, "{k}");
            assert_eq!(out.outcomes[0].drawn_lines, 1, "{k}");
            assert_eq!(paths_apart(source, &out.bytes), 0, "{k}");
            assert_eq!(rules_of(source), rules_of(&out.bytes), "{k}");
            let checked = check(source, &out.bytes, &out.outcomes, None, 1.25).unwrap();
            assert!(
                checked.occupied_lines.is_empty() && checked.crossings.is_empty(),
                "{k}"
            );
            assert!(
                checked
                    .pages
                    .iter()
                    .all(|p| p.paths_changed == 0 && p.lightened_outside == 0),
                "{k}: {:?}",
                checked.pages
            );
        }
    }

    /// An A4 page of `content` with Courier (WinAnsi, not embedded) as `/F1`, and `annots`
    /// (annotation dictionaries) describing links and other overlays.
    fn courier_page(content: &str, annots: &[&str]) -> Vec<u8> {
        let refs: Vec<String> = (0..annots.len())
            .map(|k| format!("{} 0 R", 6 + k))
            .collect();
        let mut objects = vec![
            "<< /Type /Catalog /Pages 2 0 R >>".to_owned(),
            "<< /Type /Pages /Kids [3 0 R] /Count 1 >>".to_owned(),
            format!(
                "<< /Type /Page /Parent 2 0 R /MediaBox [0 0 595 842] /Resources << /Font << \
                 /F1 4 0 R >> >> /Contents 5 0 R /Annots [{}] >>",
                refs.join(" ")
            ),
            format!(
                "<< /Type /Font /Subtype /Type1 /BaseFont /Courier /Encoding /WinAnsiEncoding \
                 /FirstChar 32 /LastChar 255 /Widths [{}] >>",
                "600 ".repeat(224)
            ),
            format!(
                "<< /Length {} >>\nstream\n{content}\nendstream",
                content.len() + 1
            ),
        ];
        objects.extend(annots.iter().map(|a| (*a).to_owned()));
        let mut out = b"%PDF-1.7\n".to_vec();
        let mut at = Vec::new();
        for (i, o) in objects.iter().enumerate() {
            at.push(out.len());
            out.extend(format!("{} 0 obj\n{o}\nendobj\n", i + 1).bytes());
        }
        let xref = out.len();
        out.extend(format!("xref\n0 {}\n0000000000 65535 f \n", objects.len() + 1).bytes());
        for a in at {
            out.extend(format!("{a:010} 00000 n \n").bytes());
        }
        out.extend(
            format!(
                "trailer\n<< /Size {} /Root 1 0 R >>\nstartxref\n{xref}\n%%EOF\n",
                objects.len() + 1
            )
            .bytes(),
        );
        out
    }

    /// "Gardening group keeps the beds green." in Courier 12 pt at 95% width from 72, 740, a
    /// 0.6 pt rule across it `offset` below the baseline, and `extra` drawn after it.
    #[cfg(feature = "pdf-erase")]
    fn ruled_line(offset: f64, extra: &str) -> Vec<u8> {
        let width = 37.0 * 7.2 * 0.95;
        let content = format!(
            "BT /F1 12 Tf 0 Tr 95 Tz 1 0 0 1 72 740 Tm (Gardening group keeps the beds green.) Tj ET\n\
             0 g 72 {} {width} 0.6 re f\n{extra}",
            740.0 - offset
        );
        courier_page(&content, &[])
    }

    /// Whether the first page of `bytes` draws a rule across at page height `y` (its middle),
    /// from `left` to `right`: in every column, a pixel within 0.3 pt of it dark (at 144 dpi, a
    /// pixel in from either end).
    #[cfg(feature = "pdf-erase")]
    fn drawn_across(bytes: &[u8], y: f64, left: f64, right: f64) -> bool {
        let band = render_band(bytes, 0, 2.0, y - 0.3, y + 0.3).unwrap();
        let w = 1190;
        let rows = band.len() / (w * 4);
        rows > 0
            && ((left * 2.0) as usize + 2..(right * 2.0) as usize - 2).all(|x| {
                (0..rows).any(|r| {
                    let k = (r * w + x) * 4;
                    band[k..k + 3].iter().all(|&c| c < 128)
                })
            })
    }

    #[test]
    #[cfg(feature = "pdf-erase")]
    #[ignore = "requires local PDFium and IPA font"]
    fn the_top_of_a_box_just_below_a_line_is_not_its_underline() {
        let translations =
            HashMap::from([("p1-0".into(), "Translated gardening information.".into())]);
        for offset in [1.2, 1.6] {
            let (low, high) = (740.0 - offset - 15.0, 740.0 - offset);
            let right = 72.0 + 37.0 * 7.2 * 0.95;
            // Test both open cells whose sides extend below the rule and closed boxes with
            // a bottom drawn as a separate stroke.
            let sides =
                format!("0 G 0.4 w 72 {low} m 72 {high} l S {right} {low} m {right} {high} l S\n");
            let closed = format!("{sides}72 {low} m {right} {low} l S\n");
            for extra in [sides, closed] {
                let source = ruled_line(offset, &extra);
                {
                    let (_guard, pdfium) = pdfium().unwrap();
                    let doc = read_doc(&pdfium, &source).unwrap();
                    let (body, leading) = document_body(&doc).unwrap();
                    let f = verified_found(doc.lib, &doc.page(0).unwrap(), 0, body, leading);
                    // The rule across the line (and the bottom, a rule of its own), none of it
                    // the line's: the rule is left as it is drawn, the line set over it if it
                    // can be, clear of it.
                    assert!(!f.scan.decorations.is_empty(), "{offset} {extra}");
                    assert!(f.kept_underlines.contains(&0), "{offset} {extra}");
                }
                // Through the file tab's pipeline, the box is drawn whole: its top across, its
                // sides (below the descenders of the line, which go with it) and its bottom as
                // they were, and no check fails.
                let out =
                    super::super::export::write(&source, None, &translations, Options::default())
                        .unwrap();
                assert_eq!(paths_apart(&source, &out.bytes), 0, "{offset} {extra}");
                assert!(
                    drawn_across(&out.bytes, 740.0 - offset + 0.3, 72.0, right),
                    "{offset} {extra}"
                );
                let (a, b) = (
                    render_band(&source, 0, 2.0, low - 1.0, high - 2.5).unwrap(),
                    render_band(&out.bytes, 0, 2.0, low - 1.0, high - 2.5).unwrap(),
                );
                assert_eq!(
                    lost_ink(&a, &b, 1190, a.len() / (1190 * 4), 4, &[]),
                    0,
                    "{offset} {extra}"
                );
                let checked = check(&source, &out.bytes, &out.outcomes, None, 1.25).unwrap();
                assert!(checked.occupied_lines.is_empty() && checked.crossings.is_empty());
                assert!(
                    checked
                        .pages
                        .iter()
                        .all(|p| p.paths_changed == 0 && p.lightened_outside == 0),
                    "{offset} {extra}: {:?}",
                    checked.pages
                );
            }
        }
    }

    /// A link annotation's `/Rect`, `/QuadPoints` (if any) and URI.
    #[cfg(feature = "pdf-erase")]
    type Link = (Vec<f64>, Option<Vec<f64>>, Vec<u8>);

    /// The first page's link annotations.
    #[cfg(feature = "pdf-erase")]
    fn links_of(bytes: &[u8]) -> Vec<Link> {
        let doc = lopdf::Document::load_mem(bytes).unwrap();
        let page = doc.get_pages()[&1];
        let numbers = |o: &lopdf::Object| -> Vec<f64> {
            o.as_array()
                .unwrap()
                .iter()
                .map(|v| f64::from(v.as_float().unwrap()))
                .collect()
        };
        let annots = doc.get_dictionary(page).unwrap().get(b"Annots").unwrap();
        doc.dereference(annots)
            .unwrap()
            .1
            .as_array()
            .unwrap()
            .iter()
            .map(|a| {
                let d = doc.get_dictionary(a.as_reference().unwrap()).unwrap();
                let uri = d
                    .get(b"A")
                    .and_then(lopdf::Object::as_dict)
                    .and_then(|a| a.get(b"URI"))
                    .and_then(lopdf::Object::as_str)
                    .map(<[u8]>::to_vec)
                    .unwrap_or_default();
                (
                    numbers(d.get(b"Rect").unwrap()),
                    d.get(b"QuadPoints").ok().map(numbers),
                    uri,
                )
            })
            .collect()
    }

    #[test]
    #[cfg(feature = "pdf-erase")]
    #[ignore = "requires local PDFium and IPA font"]
    fn a_paragraph_link_goes_to_its_translation() {
        // A link on two words with no underline, within the
        // line, in blue (as hyperref's `colorlinks` sets one).
        let line = "BT /F1 12 Tf 95 Tz 1 0 0 1 72 740 Tm (Our ) Tj 0 0 0.8 rg (gardening group) Tj \
                    0 g ( meets each week.) Tj ET\n";
        // The whole line a link, underlined end to end in blue as Word draws one (1.6 pt below
        // the baseline): set over its rule, which stays as it is drawn.
        let whole = "BT 0 0 0.8 rg /F1 12 Tf 95 Tz 1 0 0 1 72 740 Tm \
                     (Gardening group keeps the beds green.) Tj ET\n0 0 0.8 rg 72 738.4 253.08 0.6 re f\n";
        let link = |rect: &str, extra: &str| {
            format!(
                "<< /Type /Annot /Subtype /Link /Rect [{rect}] /Border [0 0 0] {extra} \
                 /A << /S /URI /URI (https://example.invalid/) >> >>"
            )
        };
        let translations =
            HashMap::from([("p1-0".into(), "Translated gardening information.".into())]);
        let one = link("99.36 736 201.96 752", "");
        let quad = link(
            "99.36 736 201.96 752",
            "/QuadPoints [99.36 752 201.96 752 99.36 736 201.96 736]",
        );
        let whole_link = link("69.75 737.6 327.28 750.1", "");
        for source in [
            courier_page(line, &[&one]),
            courier_page(line, &[&quad]),
            courier_page(whole, &[&whole_link]),
        ] {
            let out = super::super::export::write(&source, None, &translations, Options::default())
                .unwrap();
            assert_eq!(out.outcomes[0].status, Status::Translate);
            assert_eq!(paths_apart(&source, &out.bytes), 0);
            let drawn = out.outcomes[0].drawn.unwrap();
            let links = links_of(&out.bytes);
            assert_eq!(links.len(), 1);
            let (rect, quads, uri) = &links[0];
            // Display space (from the top of the 842 pt page) to page space.
            let expected = [
                drawn.left,
                842.0 - drawn.bottom,
                drawn.right,
                842.0 - drawn.top,
            ];
            assert!(
                rect.iter().zip(expected).all(|(a, b)| (a - b).abs() < 0.01),
                "{rect:?} {expected:?}"
            );
            if let Some(q) = quads {
                let [l, b, r, t] = expected;
                assert!(
                    q.iter()
                        .zip([l, t, r, t, l, b, r, b])
                        .all(|(a, b)| (a - b).abs() < 0.01)
                );
            }
            assert_eq!(uri, b"https://example.invalid/");
            // The page passes the check, with its link moved.
            let checked = check(&source, &out.bytes, &out.outcomes, None, 1.25).unwrap();
            assert!(
                checked
                    .pages
                    .iter()
                    .all(|p| p.lightened_outside == 0 && p.paths_changed == 0)
            );
        }
        // The linked words have an underline 4 pt below the
        // baseline): the rule stays as it is drawn, and so does the line, link and all.
        let underlined = format!("{line}0 0 0.8 rg 99.36 736 102.6 0.6 re f\n");
        let source = courier_page(&underlined, &[&one]);
        let out =
            super::super::export::write(&source, None, &translations, Options::default()).unwrap();
        assert_eq!(out.outcomes[0].status, Status::Retain(Retain::Decoration));
        assert_eq!(links_of(&out.bytes), links_of(&source));
        assert_eq!(paths_apart(&source, &out.bytes), 0);
        // Two links in the paragraph, or a link drawn by an appearance of its own: kept as
        // they are.
        let second = link("210 736 260 752", "");
        let with_appearance = {
            let mut doc = lopdf::Document::load_mem(&courier_page(line, &[&one])).unwrap();
            let form = doc.add_object(lopdf::Stream::new(
                lopdf::dictionary! { "Type" => "XObject", "Subtype" => "Form",
                "BBox" => vec![0.into(), 0.into(), 102.into(), 16.into()] },
                Vec::new(),
            ));
            doc.get_object_mut((6, 0))
                .unwrap()
                .as_dict_mut()
                .unwrap()
                .set("AP", lopdf::dictionary! { "N" => form });
            let mut bytes = Vec::new();
            doc.save_to(&mut bytes).unwrap();
            bytes
        };
        // A link on two words of a paragraph of two lines (a citation, say) would make the
        // whole paragraph a link: kept as it is, too.
        let lines = "BT /F1 12 Tf 1 0 0 1 72 740 Tm (Our gardening group meets each week in) Tj \
                     0 -14 Td (the shared beds behind the school.) Tj ET\n";
        // A link within the document (to a page, by `/Dest` or a `/GoTo` action, or to a
        // named place), as a one-line paragraph's citation number: it is on those words alone.
        let within = |target: &str| {
            format!(
                "<< /Type /Annot /Subtype /Link /Rect [99.36 736 201.96 752] /Border [0 0 0] \
                 {target} >>"
            )
        };
        for source in [
            courier_page(line, &[&one, &second]),
            with_appearance,
            courier_page(lines, &[&link("100.8 736 208.8 752", "")]),
            courier_page(line, &[&within("/Dest [3 0 R /XYZ 0 842 0]")]),
            courier_page(line, &[&within("/A << /S /GoTo /D [3 0 R /Fit] >>")]),
            courier_page(line, &[&within("/Dest (notes)")]),
        ] {
            let before = links_of(&source);
            let out = super::super::export::write(&source, None, &translations, Options::default())
                .unwrap();
            assert_eq!(out.outcomes[0].status, Status::Translate);
            assert_eq!(links_of(&out.bytes), before);
        }
    }

    #[test]
    #[cfg(feature = "pdf-erase")]
    #[ignore = "requires local PDFium and IPA font"]
    fn a_mark_lost_far_from_the_translation_fails_the_check() {
        // A 2 pt dot 190 pt right of the line, about 16 pixels at 90 dpi: far below the
        // 0.1% of the page that `changed_outside` lets pass, but ink lost all the same.
        let line = "BT /F1 12 Tf 1 0 0 1 72 740 Tm (Gardening group keeps the beds green.) Tj ET\n";
        let source = courier_page(&format!("{line}0 g 530 738 2 2 re f\n"), &[]);
        let without = courier_page(line, &[]);
        let translations =
            HashMap::from([("p1-0".into(), "Translated gardening information.".into())]);
        let (out, outcomes) = write(&without, None, &translations, Options::default()).unwrap();
        assert_eq!(outcomes[0].status, Status::Translate);
        let good = check(&without, &out, &outcomes, None, 1.25).unwrap();
        assert_eq!(good.pages[0].lightened_outside, 0);
        let lost = check(&source, &out, &outcomes, None, 1.25).unwrap();
        assert!(lost.pages[0].changed_outside <= 0.001, "{:?}", lost.pages);
        assert!(lost.pages[0].lightened_outside > 0, "{:?}", lost.pages);
    }

    #[test]
    #[cfg(feature = "pdf-erase")]
    #[ignore = "requires local PDFium and IPA font"]
    fn a_rule_under_a_line_stays_with_or_without_a_link() {
        // Draw an isolated rule immediately after the text that is touched by the
        // descenders (up to 2.4 pt below the baseline) or clear of them (4 pt below); with no
        // link over the line, or a link as Word puts one (2.25 pt past its ends, down to 1 pt
        // below the rule). However it is, the rule stays across the line as it is drawn, every
        // path with it; the translation, if it is set, is over it and clear of it, and every
        // check of the save passes.
        let translations =
            HashMap::from([("p1-0".into(), "Translated gardening information.".into())]);
        let right = 72.0 + 37.0 * 7.2 * 0.95;
        for offset in [1.2, 1.6, 2.4, 4.0] {
            let unlinked = ruled_line(offset, "");
            let linked = with_links(
                &unlinked,
                &[[69.75, 740.0 - offset - 1.0, right + 2.2, 750.1]],
            );
            for source in [unlinked, linked] {
                let out =
                    super::super::export::write(&source, None, &translations, Options::default())
                        .unwrap();
                assert!(
                    drawn_across(&out.bytes, 740.0 - offset + 0.3, 72.0, right),
                    "{offset}"
                );
                assert_eq!(paths_apart(&source, &out.bytes), 0, "{offset}");
                assert_eq!(rules_of(&source), rules_of(&out.bytes), "{offset}");
                let checked = check(&source, &out.bytes, &out.outcomes, None, 1.25).unwrap();
                assert!(checked.occupied_lines.is_empty() && checked.crossings.is_empty());
                assert!(checked.text.remaining.is_empty() && checked.text.lost.is_empty());
                assert!(
                    checked
                        .pages
                        .iter()
                        .all(|p| p.paths_changed == 0 && p.lightened_outside == 0),
                    "{offset}: {:?}",
                    checked.pages
                );
            }
        }
    }

    #[test]
    #[cfg(feature = "pdf-erase")]
    #[ignore = "requires local PDFium and IPA font"]
    fn a_path_drawn_otherwise_is_counted() {
        // A line, its rule, a box under it (four strokes) and a filled mark: the comparison of
        // a save's paths lets nothing pass. A rule taken off, moved half a point, made a point
        // longer, in another color, dashed, clipped, drawn under the box rather than over it,
        // or a rule drawn anew: each is a path changed.
        let text = "BT /F1 12 Tf 0 Tr 95 Tz 1 0 0 1 72 740 Tm (Gardening group keeps the beds green.) Tj ET\n";
        let rule = "0 g 72 738.4 253.08 0.6 re f\n";
        let rest = "0 G 0.4 w 72 737.4 m 72 720 l S 325.08 737.4 m 325.08 720 l S 72 720 m 325.08 720 l S\n\
                    0 g 400 700 4 4 re f\n";
        let page = |content: String| courier_page(&content, &[]);
        let source = page(format!("{text}{rule}{rest}"));
        assert_eq!(paths_apart(&source, &source), 0);
        // The text set otherwise (a translation in its place) changes no path.
        let translated = page(format!(
            "BT /F1 9 Tf 1 0 0 1 72 741 Tm (Translated information.) Tj ET\n{rule}{rest}"
        ));
        assert_eq!(paths_apart(&source, &translated), 0);
        for (name, content) in [
            ("taken off", format!("{text}{rest}")),
            (
                "moved",
                format!("{text}0 g 72 737.9 253.08 0.6 re f\n{rest}"),
            ),
            (
                "longer",
                format!("{text}0 g 72 738.4 254.08 0.6 re f\n{rest}"),
            ),
            (
                "recolored",
                format!("{text}0 0 0.8 rg 72 738.4 253.08 0.6 re f\n0 g {rest}"),
            ),
            (
                "dashed",
                format!("{text}[2 1] 0 d 0 G 0.6 w 72 738.7 m 325.08 738.7 l S [] 0 d {rest}"),
            ),
            (
                "clipped",
                format!("{text}q 72 700 120 60 re W n {rule}Q\n{rest}"),
            ),
            ("reordered", format!("{text}{rest}{rule}")),
            (
                "drawn anew",
                format!("{text}{rule}{rest}0 g 72 724.4 253.08 0.6 re f\n"),
            ),
        ] {
            assert!(paths_apart(&source, &page(content)) > 0, "{name}");
        }
    }

    #[test]
    #[cfg(feature = "pdf-erase")]
    #[ignore = "requires local PDFium and IPA font"]
    fn a_page_with_forms_nested_too_deep_to_read_is_counted() {
        // A rule drawn in Forms nested `n` deep. Up to 16 deep, the comparison reads all of it;
        // deeper, it cannot, and a page it cannot read through counts as a path changed, even
        // against itself.
        let nested = |n: usize| {
            let first = 6;
            let mut objects = vec![
                "<< /Type /Catalog /Pages 2 0 R >>".to_owned(),
                "<< /Type /Pages /Kids [3 0 R] /Count 1 >>".to_owned(),
                format!(
                    "<< /Type /Page /Parent 2 0 R /MediaBox [0 0 595 842] /Resources << /Font \
                     << /F1 4 0 R >> /XObject << /Fm {first} 0 R >> >> /Contents 5 0 R >>"
                ),
                "<< /Type /Font /Subtype /Type1 /BaseFont /Courier >>".to_owned(),
            ];
            let content = "BT /F1 12 Tf 1 0 0 1 72 740 Tm (Gardening group.) Tj ET\n/Fm Do\n";
            objects.push(format!(
                "<< /Length {} >>\nstream\n{content}\nendstream",
                content.len() + 1
            ));
            for k in 0..n {
                let (body, resources) = if k + 1 == n {
                    ("0 g 72 700 200 0.6 re f".to_owned(), String::new())
                } else {
                    (
                        "/Fm Do".to_owned(),
                        format!("/Resources << /XObject << /Fm {} 0 R >> >>", first + k + 1),
                    )
                };
                objects.push(format!(
                    "<< /Type /XObject /Subtype /Form /BBox [0 0 595 842] {resources} /Length {} \
                     >>\nstream\n{body}\nendstream",
                    body.len() + 1
                ));
            }
            let mut out = b"%PDF-1.7\n".to_vec();
            let mut at = Vec::new();
            for (i, o) in objects.iter().enumerate() {
                at.push(out.len());
                out.extend(format!("{} 0 obj\n{o}\nendobj\n", i + 1).bytes());
            }
            let xref = out.len();
            out.extend(format!("xref\n0 {}\n0000000000 65535 f \n", objects.len() + 1).bytes());
            for a in at {
                out.extend(format!("{a:010} 00000 n \n").bytes());
            }
            out.extend(
                format!(
                    "trailer\n<< /Size {} /Root 1 0 R >>\nstartxref\n{xref}\n%%EOF\n",
                    objects.len() + 1
                )
                .bytes(),
            );
            out
        };
        for (n, changed) in [(1, 0), (16, 0), (17, 1), (20, 1)] {
            let bytes = nested(n);
            assert_eq!(paths_apart(&bytes, &bytes), changed, "{n}");
        }
    }

    /// The label's rule, 1.6 pt below its baseline, is
    /// the top of a box whose sides and bottom, drawn as other paths, stop `gap` pt short of it
    /// (a printer's small corner gap): solid, dashed, dotted, thin filled rectangles, clipped,
    /// or in a Form.
    #[cfg(feature = "pdf-erase")]
    fn gapped_frame(gap: f64, style: &str) -> Vec<u8> {
        let w = 37.0 * 7.2 * 0.95;
        let (right, low) = (72.0 + w, 738.4 - gap);
        let strokes = format!(
            "0 G 0.4 w 72 {low} m 72 720 l S {right} {low} m {right} 720 l S \
             72 720 m {right} 720 l S\n"
        );
        let sides = match style {
            "dash" => format!("[2 2] 0 d {strokes}"),
            "dot" => format!("[0.2 2] 0 d 1 J {strokes}"),
            "fill" => format!(
                "72 720 0.4 {} re f {} 720 0.4 {} re f 72 720 {w} 0.4 re f\n",
                low - 720.0,
                right - 0.4,
                low - 720.0
            ),
            "clip" => format!("q 70 719 {} {} re W n {strokes}Q\n", w + 4.0, low - 719.0),
            _ => strokes,
        };
        if style != "form" {
            return ruled_line(1.6, &sides);
        }
        let mut doc = lopdf::Document::load_mem(&ruled_line(1.6, "/Fm Do\n")).unwrap();
        let form = doc.add_object(lopdf::Stream::new(
            lopdf::dictionary! { "Type" => "XObject", "Subtype" => "Form",
            "BBox" => vec![0.into(), 0.into(), 595.into(), 842.into()],
            "Resources" => lopdf::dictionary! {} },
            sides.into_bytes(),
        ));
        doc.get_object_mut((3, 0))
            .unwrap()
            .as_dict_mut()
            .unwrap()
            .get_mut(b"Resources")
            .unwrap()
            .as_dict_mut()
            .unwrap()
            .set("XObject", lopdf::dictionary! { "Fm" => form });
        let mut bytes = Vec::new();
        doc.save_to(&mut bytes).unwrap();
        bytes
    }

    #[test]
    #[cfg(feature = "pdf-erase")]
    #[ignore = "requires local PDFium and IPA font"]
    fn a_box_whose_corners_have_small_gaps_keeps_its_top() {
        // Check that the shorter translation preserves the whole box. Mistaking the top for
        // the label's underline would erase the right end of the rule.
        let translations =
            HashMap::from([("p1-0".into(), "Translated gardening information.".into())]);
        let right = 72.0 + 37.0 * 7.2 * 0.95;
        for (gap, style) in [
            (1.0, "solid"),
            (1.0, "dash"),
            (1.2, "dot"),
            (1.5, "fill"),
            (1.0, "clip"),
            (1.0, "form"),
            (2.0, "solid"),
        ] {
            let source = gapped_frame(gap, style);
            let out = super::super::export::write(&source, None, &translations, Options::default())
                .unwrap();
            // The top: the row of pixels in the middle of the rule is ink from end to end.
            let row = render_band(&out.bytes, 0, 2.0, 738.5, 738.9).unwrap();
            assert_eq!(row.len(), 1190 * 4, "{gap} {style}");
            for x in (72 * 2 + 2)..((right * 2.0) as usize - 2) {
                assert!(
                    row[x * 4..x * 4 + 3].iter().all(|&c| c < 128),
                    "{gap} {style}: the top is lost at {x}"
                );
            }
            // The sides and the bottom: no pixel turned lighter.
            let (a, b) = (
                render_band(&source, 0, 2.0, 719.0, 735.0).unwrap(),
                render_band(&out.bytes, 0, 2.0, 719.0, 735.0).unwrap(),
            );
            assert_eq!(a.len(), b.len());
            assert_eq!(
                lost_ink(&a, &b, 1190, a.len() / (1190 * 4), 4, &[]),
                0,
                "{gap} {style}"
            );
        }
    }

    #[test]
    #[cfg(feature = "pdf-erase")]
    #[ignore = "requires local PDFium and IPA font"]
    fn outline_and_faint_sources_are_retained_before_erasure() {
        for (prefix, alpha) in [("1 Tr\n", None), ("", Some(0.08))] {
            let source = decorate_sample(prefix, "", alpha);
            let translations = HashMap::from([("p1-1".into(), "A visible translation.".into())]);
            let (plan, chars) =
                erase_plan(&source, None, &translations, Options::default()).unwrap();
            assert_eq!(plan[1].status, Status::Retain(Retain::Paint));
            assert!(chars.is_empty());
        }
    }

    #[test]
    #[cfg(feature = "pdf-erase")]
    #[ignore = "requires local PDFium and IPA font"]
    fn inconsistent_erasure_and_insertion_restore_the_original_page() {
        let source = sample();
        let map = HashMap::from([("p1-1".into(), "Translated paragraph.".into())]);
        let (clean, _) = erase_pdfium(&source, None, &map, Options::default()).unwrap();
        let (output, outcomes) = write_checked_erased(
            &source,
            &clean,
            None,
            &HashMap::new(),
            Options::default(),
            &["p1-1".into()],
        )
        .unwrap();
        assert!(outcomes.iter().all(|o| o.status != Status::Translate));
        let (mismatches, pixels) = check_erasure(&source, &output, &[], None).unwrap();
        assert!(mismatches.is_empty());
        assert!(pixels.iter().all(|p| p.changed_outside == 0.0));
    }

    #[test]
    #[cfg(feature = "pdf-erase")]
    #[ignore = "requires local PDFium and IPA font"]
    fn application_pipeline_preserves_inputs_and_checks_the_final_placement() {
        let source = sample();
        let map = HashMap::from([("p1-1".into(), "Translated paragraph.".into())]);
        let saved = map.clone();
        let result = super::super::export::write(&source, None, &map, Options::default()).unwrap();
        assert_eq!(map, saved);
        assert_eq!(result.outcomes[1].status, Status::Retain(Retain::Erase));
        assert!(result.erasure.erased.is_empty());
        assert!(result.erasure.retained.contains_key("p1-1"));
        let audit = check(&source, &result.bytes, &result.outcomes, None, 1.25).unwrap();
        assert!(audit.accounted());
        assert!(audit.occupied_lines.is_empty());
        assert!(audit.invisible_lines.is_empty());
    }

    #[test]
    #[cfg(feature = "pdf-erase")]
    #[ignore = "requires local PDFium and IPA font"]
    fn application_fallback_keeps_original_pixels_and_routes_every_translation_once() {
        let source = sample();
        let file = tempfile::Builder::new().suffix(".pdf").tempfile().unwrap();
        std::fs::write(file.path(), &source).unwrap();
        let doc = super::super::export::Document::read(file.path()).unwrap();
        let targets: Vec<_> = doc
            .units()
            .iter()
            .enumerate()
            .map(|(i, _)| Some(format!("Translated paragraph {i}.")))
            .collect();
        let result = doc.write(&targets, "en").unwrap();
        assert_eq!(
            result.pages[0].kind,
            super::super::export::PageKind::Bilingual
        );
        assert_eq!(result.pages[0].appended.len(), targets.len());
        assert!(result.pages[0].placed.is_empty());
        assert!(result.pages[0].added_pages > 0);
        let (changed, pixels) = check_erasure(&source, &result.bytes, &[], Some(1)).unwrap();
        assert!(changed.is_empty());
        assert!(pixels.iter().all(|p| p.changed_outside == 0.0));
        let (_guard, pdfium) = pdfium().unwrap();
        let out = read_doc(&pdfium, &result.bytes).unwrap();
        let text = page_text(out.lib, &out.page(1).unwrap());
        for t in targets.iter().flatten() {
            let normalized: String = t.chars().filter(|c| !c.is_whitespace()).collect();
            assert_eq!(text.matches(&normalized).count(), 1, "{text}");
        }
    }

    #[test]
    #[ignore = "requires local PDFium and IPA font"]
    fn application_missing_glyphs_keep_unicode_and_unchanged_targets_add_no_pages() {
        let file = tempfile::Builder::new().suffix(".pdf").tempfile().unwrap();
        std::fs::write(file.path(), sample()).unwrap();
        let doc = super::super::export::Document::read(file.path()).unwrap();
        let targets: Vec<_> = doc
            .units()
            .iter()
            .enumerate()
            .map(|(i, _)| Some(format!("Translated {i}: ™ µ 한글 😀.")))
            .collect();
        let result = doc.write(&targets, "en").unwrap();
        assert!(result.pages.iter().any(|p| p.has_square_markers));
        let font = output_font().unwrap();
        let noto = super::super::Noto::default();
        let faces = super::super::Faces::new(&font, &noto, super::super::Family::Ipa).unwrap();
        let face = &faces.fonts[0];
        assert_eq!(
            faces.glyph('µ', true).map(|(_, g)| g),
            face.glyph_index('μ')
        );
        assert!(faces.glyph('µ', true).map(|(_, g)| g).is_some());
        {
            let (_guard, pdfium) = pdfium().unwrap();
            let out = read_doc(&pdfium, &result.bytes).unwrap();
            let text = page_text(out.lib, &out.page(1).unwrap());
            assert!(
                text.contains(
                    "□marksacharacterunavailableinthisPDF'sfont,orthepositionofaformula."
                )
            );
            for t in targets.iter().flatten() {
                let normalized: String = t.chars().filter(|c| !c.is_whitespace()).collect();
                assert_eq!(text.matches(&normalized).count(), 1, "{text}");
            }
        }
        let cached = doc.write_cached(&targets, "en").unwrap();
        let again = doc.write_cached(&targets, "en").unwrap();
        assert!(std::sync::Arc::ptr_eq(&cached, &again));
        let mut changed = targets.clone();
        changed[0] = Some("A different translated title.".into());
        assert!(!std::sync::Arc::ptr_eq(
            &cached,
            &doc.write_cached(&changed, "en").unwrap()
        ));
        let unchanged: Vec<_> = doc
            .units()
            .iter()
            .map(|p| Some(p.text.to_uppercase().replace(' ', "  ")))
            .collect();
        let result = doc.write(&unchanged, "en").unwrap();
        assert!(
            result
                .pages
                .iter()
                .all(|p| p.added_pages == 0 && p.appended.is_empty())
        );
        assert_eq!(
            result
                .pages
                .iter()
                .map(|p| p.untranslated.len() + p.repeated.len())
                .sum::<usize>(),
            unchanged.len()
        );
    }

    #[test]
    #[ignore = "requires local PDFium and IPA font"]
    fn unchanged_body_counts_against_coverage_and_forms_are_separate() {
        use lopdf::dictionary;
        let source = {
            let mut pdf = lopdf::Document::load_mem(&sample()).unwrap();
            let page = pdf.get_pages()[&1];
            let mut resources = pdf.get_page_resources(page).unwrap().0.unwrap().clone();
            let fonts = pdf.get_page_fonts(page).unwrap();
            let name = String::from_utf8(fonts.keys().next().unwrap().clone()).unwrap();
            let form = pdf.add_object(lopdf::Stream::new(lopdf::dictionary! {
                "Type" => "XObject", "Subtype" => "Form", "BBox" => vec![0.into(),0.into(),600.into(),800.into()],
                "Resources" => resources.clone(),
            }, format!("BT /{name} 10 Tf 1 0 0 1 72 420 Tm (Chart legend) Tj ET 70 395 8 40 re f 90 395 8 30 re f 110 395 8 35 re f").into_bytes()));
            resources.set("XObject", lopdf::dictionary! {"ChartFigure" => form});
            pdf.get_object_mut(page)
                .unwrap()
                .as_dict_mut()
                .unwrap()
                .set("Resources", resources);
            let contents: Vec<lopdf::Object> = pdf
                .get_page_contents(page)
                .into_iter()
                .map(Into::into)
                .collect();
            pdf.get_object_mut(page)
                .unwrap()
                .as_dict_mut()
                .unwrap()
                .set("Contents", contents);
            pdf.add_page_contents(page, b"\n/ChartFigure Do\n".to_vec())
                .unwrap();
            let mut bytes = Vec::new();
            pdf.save_to(&mut bytes).unwrap();
            bytes
        };
        let file = tempfile::Builder::new().suffix(".pdf").tempfile().unwrap();
        std::fs::write(file.path(), &source).unwrap();
        let doc = super::super::export::Document::read(file.path()).unwrap();
        assert_eq!(doc.paragraphs().iter().filter(|p| p.figure).count(), 2);
        let targets = vec![Some("A translated sentence.".into()); doc.units().len()];
        let out = doc.write(&targets, "en").unwrap();
        let figure = doc
            .paragraphs()
            .iter()
            .find(|p| p.text == "Chart legend")
            .unwrap();
        assert!(out.pages[0].appended.contains(&figure.id));
        assert_eq!(
            out.pages[0].figure_characters,
            doc.paragraphs()
                .iter()
                .filter(|p| p.figure)
                .map(|p| p.text.chars().filter(|c| !c.is_whitespace()).count())
                .sum::<usize>()
        );
        assert_eq!(
            out.pages[0].target_characters,
            doc.paragraphs()
                .iter()
                .filter(|p| p.prose)
                .map(|p| p.text.chars().filter(|c| !c.is_whitespace()).count())
                .sum::<usize>()
        );
        let echo: Vec<_> = doc.units().iter().map(|p| Some(p.text.clone())).collect();
        let out = doc.write(&echo, "en").unwrap();
        assert_eq!(out.pages[0].untranslated.len(), doc.units().len());
        assert!(out.pages[0].target_characters > 0);
        assert_eq!(out.pages[0].placed_characters, 0);
        assert!(out.pages[0].reasons.contains(&"coverage"));
    }

    #[test]
    #[ignore = "requires local PDFium and IPA font"]
    fn application_reports_structural_fallback_and_consolidates_repeated_headers() {
        let source = sample();
        let parsed = lopdf::Document::load_mem(&source).unwrap();
        let root = parsed.trailer.get(b"Root").unwrap().as_reference().unwrap();
        let prev = String::from_utf8_lossy(&source)
            .rsplit_once("startxref")
            .unwrap()
            .1
            .split_whitespace()
            .next()
            .unwrap()
            .to_owned();
        let id = parsed.max_id + 1;
        let mut broken = source.clone();
        broken.extend_from_slice(format!("\nxref\n{id} 1\n0000000000 00000 n \ntrailer\n<< /Size {} /Root {} {} R /Prev {prev} >>\nstartxref\n{}\n%%EOF\n", id+1,root.0,root.1,source.len()+1).as_bytes());
        let file = tempfile::Builder::new().suffix(".pdf").tempfile().unwrap();
        std::fs::write(file.path(), &broken).unwrap();
        let doc = super::super::export::Document::read(file.path()).unwrap();
        let targets: Vec<_> = doc
            .units()
            .iter()
            .map(|_| Some("Translated contents.".into()))
            .collect();
        let out = doc.write(&targets, "en").unwrap();
        assert!(out.fallback_error.as_ref().unwrap().contains("missing"));
        assert!(
            out.pages
                .iter()
                .all(|p| p.kind == super::super::export::PageKind::Bilingual)
        );
        let (changed, pixels) = check_erasure(&source, &out.bytes, &[], Some(1)).unwrap();
        assert!(changed.is_empty());
        assert!(pixels.iter().all(|p| p.changed_outside == 0.0));
        let repeated = {
            let (_guard, pdfium) = pdfium().unwrap();
            let original = pdfium.load_pdf_from_byte_slice(&source, None).unwrap();
            let mut repeated = pdfium.create_new_pdf().unwrap();
            repeated
                .pages_mut()
                .copy_page_from_document(&original, 0, 0)
                .unwrap();
            repeated
                .pages_mut()
                .copy_page_from_document(&original, 0, 1)
                .unwrap();
            repeated.save_to_bytes().unwrap()
        };
        std::fs::write(file.path(), &repeated).unwrap();
        let doc = super::super::export::Document::read(file.path()).unwrap();
        let targets: Vec<_> = doc
            .units()
            .iter()
            .map(|_| Some("Translated contents.".into()))
            .collect();
        let out = doc.write(&targets, "en").unwrap();
        assert!(out.pages.iter().all(|p| p.repeated.is_empty()));
        let repeated = {
            let (_guard, pdfium) = pdfium().unwrap();
            let original = pdfium.load_pdf_from_byte_slice(&source, None).unwrap();
            let mut repeated = pdfium.create_new_pdf().unwrap();
            for page in 0..3 {
                repeated
                    .pages_mut()
                    .copy_page_from_document(&original, 0, page)
                    .unwrap();
            }
            repeated.save_to_bytes().unwrap()
        };
        std::fs::write(file.path(), &repeated).unwrap();
        let doc = super::super::export::Document::read(file.path()).unwrap();
        let targets = vec![Some("Translated contents.".into()); doc.units().len()];
        let out = doc.write(&targets, "en").unwrap();
        assert!(out.pages.iter().all(|p| !p.repeated.is_empty()));
        assert!(
            out.pages
                .iter()
                .all(|p| p.repeated.iter().any(|r| r.id.ends_with("-0")))
        );
    }

    /// A page with two paragraphs of two lines, a heading, and a box drawn below them.
    fn sample() -> Vec<u8> {
        let (_guard, pdfium) = pdfium().unwrap();
        let mut doc = pdfium.create_new_pdf().unwrap();
        let font = doc.fonts_mut().helvetica();
        let mut page = doc
            .pages_mut()
            .create_page_at_end(PdfPagePaperSize::a4())
            .unwrap();
        let lines = [
            (72.0, 760.0, 16.0, "In-place translation"),
            (
                72.0,
                730.0,
                11.0,
                "The first paragraph is long enough to take two lines of",
            ),
            (
                72.0,
                717.0,
                11.0,
                "text, and it ends here on the second line.",
            ),
            (
                84.0,
                690.0,
                11.0,
                "The second paragraph is indented on its first line and",
            ),
            (72.0, 677.0, 11.0, "goes on below it, where it ends too."),
        ];
        for (x, y, size, text) in lines {
            page.objects_mut()
                .create_text_object(
                    PdfPoints::new(x),
                    PdfPoints::new(y),
                    text,
                    font,
                    PdfPoints::new(size),
                )
                .unwrap();
        }
        page.objects_mut()
            .create_path_object_rect(
                PdfRect::new_from_values(560.0, 72.0, 640.0, 520.0),
                Some(PdfColor::new(0, 0, 200, 255)),
                Some(PdfPoints::new(2.0)),
                Some(PdfColor::new(230, 230, 250, 255)),
            )
            .unwrap();
        drop(page);
        doc.save_to_bytes().unwrap()
    }

    #[test]
    #[cfg(feature = "pdf-erase")]
    #[cfg(feature = "pdf-external-tests")]
    #[ignore = "requires local PDFium, IPA Gothic, and pdftotext"]
    fn shared_glyphs_and_supplementary_unicode_round_trip_exactly() {
        let source = sample();
        let p = analyze(&source, None).unwrap().analyzed[0].paragraphs[1].clone();
        let pairs = [
            '\u{a0}',
            '\u{ad}',
            '\u{2dc}',
            '\u{ffe0}',
            '\u{ffe1}',
            '\u{ffe5}',
            '\u{ffe2}',
            '\u{ff65}',
            '\u{2015}',
            '\u{ffe3}',
            '\u{ff5e}',
            '\u{29fd7}',
        ];
        let font_bytes = inplace_font().unwrap();
        let face = Face::parse(&font_bytes).unwrap();
        for &ch in &pairs {
            let a = face.fonts.fonts[0].glyph_index(ch).unwrap();
            let b = face.fonts.fonts[0].glyph_index(same_glyph(ch)).unwrap();
            eprintln!(
                "U+{:04X}/U+{:04X}: glyphs {}/{} shared={}",
                ch as u32,
                same_glyph(ch) as u32,
                a.0,
                b.0,
                a == b
            );
        }
        let text = format!(
            "⚫➢▪◼•◦∗確認{}終了",
            pairs
                .iter()
                .flat_map(|&c| [same_glyph(c), c])
                .map(|c| format!("[{c}]"))
                .collect::<String>()
        );
        // Use a wide synthetic frame so all 24 characters fit on one line unchanged.
        let font = output_font().unwrap();
        let (guard, pdfium) = super::super::pdfium().unwrap();
        let mut blank = pdfium.create_new_pdf().unwrap();
        blank
            .pages_mut()
            .create_page_at_end(PdfPagePaperSize::a3())
            .unwrap();
        let bytes = blank.save_to_bytes().unwrap();
        drop(blank);
        drop(pdfium);
        drop(guard);
        let lines = [super::super::unicode::TextLine {
            page: 0,
            text: text.clone(),
            size: 10.0,
            matrix: [1.0, 0.0, 0.0, 1.0, 20.0, 700.0],
            color: [0, 0, 0, 255],
        }];
        let out = super::super::unicode::append(&bytes, &font, &lines).unwrap();
        let (guard, pdfium) = super::super::pdfium().unwrap();
        let doc = Doc::load(pdfium.bindings(), &out).unwrap();
        let page = doc.page(0).unwrap();
        let actual = raw_page_text(doc.lib, &page);
        assert_eq!(actual, text);
        drop(page);
        drop(doc);
        drop(pdfium);
        drop(guard);
        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().join("unicode.pdf");
        std::fs::write(&path, &out).unwrap();
        let poppler = std::process::Command::new("pdftotext")
            .arg("-raw")
            .arg(&path)
            .arg("-")
            .output()
            .unwrap();
        assert!(poppler.status.success());
        assert_eq!(
            String::from_utf8(poppler.stdout)
                .unwrap()
                .trim_end_matches(['\n', '\u{c}']),
            text
        );
        // Also cover the actual paragraph insertion path with both shared hyphens and CJK.
        let short = "A[ ][\u{a0}]-\u{ad}~\u{2dc}¢￠£￡¥￥¬￢·･—―‾￣〜～𩿎𩿗B";
        let (out, outcomes) = write(
            &source,
            None,
            &HashMap::from([(p.id, short.into())]),
            Options::default(),
        )
        .unwrap();
        assert_eq!(outcomes[1].translation.as_deref(), Some(short));
        std::fs::write(&path, &out).unwrap();
        let poppler = std::process::Command::new("pdftotext")
            .arg("-raw")
            .arg(&path)
            .arg("-")
            .output()
            .unwrap();
        let extracted = String::from_utf8(poppler.stdout).unwrap();
        assert!(extracted.contains(short), "{extracted:?}");
        let check = check(&source, &out, &outcomes, None, 1.25).unwrap();
        assert!(check.text.missing.is_empty());
    }

    #[test]
    #[ignore = "requires the local PDFium library and IPA Gothic font"]
    fn finds_the_paragraphs_of_a_page() {
        let analysis = analyze(&sample(), None).unwrap();
        let texts: Vec<_> = analysis.analyzed[0]
            .paragraphs
            .iter()
            .map(|p| (p.text.as_str(), p.status))
            .collect();
        assert_eq!(
            texts,
            [
                ("In-place translation", Status::Translate),
                (
                    "The first paragraph is long enough to take two lines of text, and it ends here on the second line.",
                    Status::Translate
                ),
                (
                    "The second paragraph is indented on its first line and goes on below it, where it ends too.",
                    Status::Translate
                ),
            ]
        );
    }

    #[test]
    #[ignore = "requires the local PDFium library and IPA Gothic font"]
    fn writes_translations_in_place_and_keeps_the_drawing() {
        let source = sample();
        let analysis = analyze(&source, None).unwrap();
        let ids: Vec<_> = analysis.analyzed[0]
            .paragraphs
            .iter()
            .map(|p| p.id.clone())
            .collect();
        let translations: HashMap<String, String> = [
            (ids[0].clone(), "その場で訳す".to_owned()),
            (
                ids[1].clone(),
                "最初の段落は二行にわたる長さがあり、二行目のここで終わる。".to_owned(),
            ),
            // Far too long for its box: kept as it is.
            (ids[2].clone(), "長すぎる訳文。".repeat(400)),
        ]
        .into();
        let (output, outcomes) = write(&source, None, &translations, Options::default()).unwrap();
        let statuses: Vec<_> = outcomes.iter().map(|o| o.status).collect();
        assert_eq!(
            statuses,
            [
                Status::Translate,
                Status::Translate,
                Status::Retain(Retain::Overflow)
            ]
        );
        let check = check(&source, &output, &outcomes, None, 1.25).unwrap();
        assert_eq!((check.pages_original, check.pages_output), (1, 1));
        let t = &check.text;
        assert!(t.missing.is_empty(), "{:?}", t.missing);
        assert!(t.remaining.is_empty(), "{:?}", t.remaining);
        assert!(t.unverified.is_empty(), "{:?}", t.unverified);
        assert!(t.lost.is_empty(), "{:?}", t.lost);
        assert!(check.overlaps.is_empty(), "{:?}", check.overlaps);
        assert!(check.accounted(), "{check:?}");
        assert_eq!(check.extracted, 3);
        assert!(check.worst_outside() <= 0.001, "{}", check.worst_outside());
        assert_eq!((check.translated, check.retained, check.skipped), (2, 1, 0));
        // The paragraph kept is still there, and the others' source text is not.
        let (_guard, pdfium) = pdfium().unwrap();
        let doc = pdfium.load_pdf_from_byte_slice(&output, None).unwrap();
        let text = doc.pages().first().unwrap().text().unwrap().all();
        assert!(text.contains("The second paragraph"));
        assert!(!text.contains("The first paragraph"));
        assert!(!text.contains("In-place"));
    }

    #[test]
    #[ignore = "requires the local PDFium library and IPA Gothic font"]
    fn separate_erasure_and_insertion_preserve_layout_and_text() {
        let source = sample();
        let translations = HashMap::from([
            ("p1-0".into(), "その場で訳す".into()),
            ("p1-1".into(), "短い訳文です。".into()),
            ("p1-2".into(), "長すぎる訳文。".repeat(400)),
        ]);
        let options = Options::default();
        let (planned, chars) = erase_plan(&source, None, &translations, options).unwrap();
        assert!(!chars.is_empty());
        assert!(chars.iter().all(|c| c.paragraph != "p1-2"));
        let (together, expected) = write(&source, None, &translations, options).unwrap();
        let (erased, _) = erase_pdfium(&source, None, &translations, options).unwrap();
        let (separate, actual) =
            write_erased(&source, &erased, None, &translations, options).unwrap();
        for ((p, a), e) in planned.iter().zip(&actual).zip(&expected) {
            assert_eq!(p.status, e.status);
            assert_eq!(a.status, e.status);
            assert_eq!(a.drawn, e.drawn);
            assert_eq!(a.scale, e.scale);
        }
        let (_guard, pdfium) = pdfium().unwrap();
        let a = Doc::load(pdfium.bindings(), &together).unwrap();
        let b = Doc::load(pdfium.bindings(), &separate).unwrap();
        let pa = a.page(0).unwrap();
        let pb = b.page(0).unwrap();
        assert_eq!(page_text(a.lib, &pa), page_text(b.lib, &pb));
        assert_eq!(render(a.lib, &pa, 1.25), render(b.lib, &pb, 1.25));
    }

    /// A page 600 × 800 of two lines of text (baselines 100 and 112 from the top, x 100 to
    /// about 267), and a Form drawn with `matrix`, of `form` (its content).
    fn with_form(matrix: &str, form: &str) -> Vec<u8> {
        let page = "BT /F1 10 Tf 1 0 0 1 100 700 Tm (The first paragraph is long enough for) Tj ET\n\
                    BT /F1 10 Tf 1 0 0 1 100 688 Tm (two lines of its own.) Tj ET\n/X1 Do\n";
        let objects = [
            "<< /Type /Catalog /Pages 2 0 R >>".to_owned(),
            "<< /Type /Pages /Kids [3 0 R] /Count 1 >>".to_owned(),
            "<< /Type /Page /Parent 2 0 R /MediaBox [0 0 600 800] /Resources << /Font << /F1 4 0 R >> /XObject << /X1 6 0 R >> >> /Contents 5 0 R >>".to_owned(),
            "<< /Type /Font /Subtype /Type1 /BaseFont /Helvetica >>".to_owned(),
            format!("<< /Length {} >>\nstream\n{page}endstream", page.len()),
            format!(
                "<< /Type /XObject /Subtype /Form /BBox [0 0 600 800] /Matrix [{matrix}] /Resources << /Font << /F1 4 0 R >> >> /Length {} >>\nstream\n{form}endstream",
                form.len()
            ),
        ];
        let mut out = b"%PDF-1.7\n".to_vec();
        let mut at = Vec::new();
        for (i, o) in objects.iter().enumerate() {
            at.push(out.len());
            out.extend(format!("{} 0 obj\n{o}\nendobj\n", i + 1).bytes());
        }
        let xref = out.len();
        out.extend(format!("xref\n0 {}\n0000000000 65535 f \n", objects.len() + 1).bytes());
        for a in at {
            out.extend(format!("{a:010} 00000 n \n").bytes());
        }
        out.extend(
            format!(
                "trailer\n<< /Size {} /Root 1 0 R >>\nstartxref\n{xref}\n%%EOF\n",
                objects.len() + 1
            )
            .bytes(),
        );
        out
    }

    /// Written with `--extend` and a translation far longer than its source: where it went, and
    /// what the check says of a write that had gone down to the foot of the box.
    fn extended(source: &[u8]) -> (Outcome, Check) {
        let analysis = analyze(source, None).unwrap();
        assert_eq!(analysis.analyzed[0].paragraphs.len(), 1);
        let id = analysis.analyzed[0].paragraphs[0].id.clone();
        let translations: HashMap<String, String> =
            [(id, "A translation sentence. ".repeat(16))].into();
        let options = Options {
            extend: true,
            ..Options::default()
        };
        let (output, outcomes) = write(source, None, &translations, options).unwrap();
        let mut over = outcomes[0].clone();
        over.status = Status::Translate;
        over.translation = Some("x".into());
        over.drawn = Some(Rect {
            bottom: 282.0,
            ..over.paragraph.rect
        });
        let check = check(source, &output, &[over], None, 1.25).unwrap();
        (outcomes[0].clone(), check)
    }

    #[test]
    #[cfg(feature = "pdf-erase")]
    #[ignore = "requires the local PDFium library and IPA Gothic font"]
    fn lettering_in_an_image_under_the_source_is_not_overprinted() {
        use lopdf::{Object, Stream, dictionary};
        let source = with_form("1 0 0 1 0 0", "");
        let mut doc = lopdf::Document::load_mem(&source).unwrap();
        let image = doc.add_object(Stream::new(
            dictionary! {
                "Type" => "XObject", "Subtype" => "Image", "Width" => 2, "Height" => 1,
                "ColorSpace" => "DeviceGray", "BitsPerComponent" => 8,
            },
            vec![200, 255],
        ));
        let page = doc.get_pages()[&1];
        doc.get_object_mut(page)
            .unwrap()
            .as_dict_mut()
            .unwrap()
            .get_mut(b"Resources")
            .unwrap()
            .as_dict_mut()
            .unwrap()
            .get_mut(b"XObject")
            .unwrap()
            .as_dict_mut()
            .unwrap()
            .set("Shadow", image);
        let original = doc.get_page_contents(page)[0];
        let extra = doc.add_object(Stream::new(
            dictionary! {},
            b"q 190 0 0 30 95 680 cm /Shadow Do Q".to_vec(),
        ));
        doc.get_object_mut(page)
            .unwrap()
            .as_dict_mut()
            .unwrap()
            .set(
                "Contents",
                vec![Object::Reference(extra), Object::Reference(original)],
            );
        let mut bytes = Vec::new();
        doc.save_to(&mut bytes).unwrap();
        let (outcome, _) = extended(&bytes);
        assert_eq!(outcome.status, Status::Retain(Retain::Overflow));
        assert!(outcome.drawn.is_none());
    }

    #[test]
    #[cfg(feature = "pdf-erase")]
    #[ignore = "requires the local PDFium library and IPA Gothic font"]
    fn annotation_rectangles_stop_expansion_even_without_an_appearance() {
        use lopdf::{Object, dictionary};
        let source = with_form("1 0 0 1 0 0", "");
        let mut doc = lopdf::Document::load_mem(&source).unwrap();
        let page = doc.get_pages()[&1];
        let annotation = doc.add_object(dictionary! {
            "Type" => "Annot", "Subtype" => "Link",
            "Rect" => vec![Object::Integer(90), 650.into(), 320.into(), 680.into()],
        });
        doc.get_object_mut(page)
            .unwrap()
            .as_dict_mut()
            .unwrap()
            .set("Annots", vec![Object::Reference(annotation)]);
        let mut bytes = Vec::new();
        doc.save_to(&mut bytes).unwrap();
        let (outcome, _) = extended(&bytes);
        if let Some(r) = outcome.drawn {
            assert!(r.bottom <= 120.0);
        } else {
            assert_eq!(outcome.status, Status::Retain(Retain::Overflow));
        }
        assert!(
            analyze(&bytes, None).unwrap().analyzed[0]
                .obstacles
                .iter()
                .any(|r| (r.top - 120.0).abs() < 0.001 && (r.bottom - 150.0).abs() < 0.001)
        );
    }

    #[test]
    #[ignore = "requires the local PDFium library and IPA Gothic font"]
    fn a_paragraph_in_a_box_stops_at_what_the_box_holds() {
        // A box (a Form, moved 60 pt down by its matrix) around the paragraph, with rotated text
        // in it below the paragraph's foot: 158 to 170 across, from about 130 down to 240.
        let rotated = with_form(
            "1 0 0 1 0 -60",
            "0.2 w 90 500 230 280 re S\nBT /F1 12 Tf 0 1 -1 0 170 560 Tm (ROTATED OBSTACLE) Tj ET\n",
        );
        let analysis = analyze(&rotated, None).unwrap();
        let obstacle = analysis.analyzed[0]
            .obstacles
            .iter()
            .find(|o| o.left > 150.0 && o.right < 175.0)
            .copied()
            .expect("the rotated text in the Form");
        assert!((obstacle.bottom - 300.0).abs() < 2.0, "{obstacle:?}");
        let (outcome, check) = extended(&rotated);
        if let Some(d) = outcome.drawn {
            assert!(d.bottom <= obstacle.top, "{d:?} over {obstacle:?}");
        }
        assert_eq!(check.crossings.len(), 1, "{check:?}");
        // A box with a line across it below the paragraph: a table's two cells.
        let grid = with_form(
            "1 0 0 1 0 0",
            "0.2 w 90 500 230 220 re S\n90 650 m 320 650 l S\n",
        );
        let (outcome, check) = extended(&grid);
        if let Some(d) = outcome.drawn {
            assert!(d.bottom <= 150.0, "{d:?}");
        }
        assert_eq!(check.crossings.len(), 1, "{check:?}");
        // The box alone: down to (1 pt above) its foot, 300.
        let (outcome, _) = extended(&with_form("1 0 0 1 0 0", "0.2 w 90 500 230 220 re S\n"));
        let d = outcome.drawn.expect("the translation, drawn");
        assert!(d.bottom > 150.0 && d.bottom <= 299.5, "{d:?}");
    }

    /// A page of pieces (one text object each), without PDFium: `(text, x, right, baseline,
    /// size)`.
    fn found(pieces: &[(&str, f64, f64, f64, f64)], drawings: Vec<Rect>) -> Found {
        let items: Vec<Item> = pieces
            .iter()
            .enumerate()
            .map(|(id, &(s, x, right, y, size))| Item {
                str: s.into(),
                x,
                y,
                w: right - x,
                size,
                top: y - 0.8 * size,
                bottom: y + 0.2 * size,
                font: "f1".into(),
                bold: false,
                italic: false,
                mono: false,
                serif: true,
                math: false,
                id,
            })
            .collect();
        let origins = items
            .iter()
            .map(|it| Origin {
                object: it.id + 1,
                raw: it.str.replace(' ', ""),
                count: it.str.replace(' ', "").chars().count(),
                color: [0, 0, 0, 255],
                invisible: false,
                unsafe_paint: false,
                scale: 1.0,
            })
            .collect::<Vec<_>>();
        let scan = Scan {
            frame: Frame {
                left: 0.0,
                top: 800.0,
                w: 600.0,
                h: 800.0,
                turns: 0,
                width: 600.0,
                height: 800.0,
            },
            rotated: false,
            totals: origins.iter().map(|o| (o.object, o.count)).collect(),
            top_level: origins.iter().map(|o| o.object).collect(),
            text_bounds: Vec::new(),
            forms: Vec::new(),
            items,
            origins,
            drawings,
            pictures: Vec::new(),
            decorations: Vec::new(),
            decoration_colors: HashMap::new(),
            links: Vec::new(),
        };
        find(scan, 0, 10.0)
    }

    const TWO_LINES: [(&str, f64, f64, f64, f64); 2] = [
        (
            "The first paragraph is long enough for",
            100.0,
            300.0,
            100.0,
            10.0,
        ),
        ("two lines of its own.", 100.0, 220.0, 112.0, 10.0),
    ];

    /// `lines` of `TWO_LINES` with thin rules (`(rect, color)`) and link annotations over it, as
    /// the save takes them ([`kept_with_rules`]).
    fn ruled_found(lines: usize, rules: &[(Rect, [u32; 4])], links: &[Rect]) -> Found {
        let f = found(&TWO_LINES[..lines], Vec::new());
        let mut scan = f.scan;
        scan.links = links.to_vec();
        for (k, (r, color)) in rules.iter().enumerate() {
            let object = 1000 + k;
            scan.decorations.push((object, *r));
            scan.decoration_colors.insert(object, *color);
            scan.drawings.push(*r);
        }
        kept_with_rules(find(scan, 0, 10.0))
    }

    #[test]
    fn no_rule_is_ever_a_paragraphs() {
        let rect = |left, top, right, bottom| Rect {
            left,
            top,
            right,
            bottom,
        };
        let black = [0, 0, 0, 255];
        // Under "The first paragraph is long enough for" (100 to 300, baseline 100, size 10).
        let rule = rect(100.0, 101.5, 300.0, 102.1);
        // Word's link annotation over the line (2.25 pt past its ends), or none.
        let word = rect(97.75, 91.0, 302.2, 103.5);
        for links in [&[][..], &[word]] {
            // One line, underlined end to end (in one piece, or in two of one color end to
            // end): the translation may be set over the rule, which stays; nothing of the page
            // but the line's text is the paragraph's.
            let first = rect(100.0, 101.5, 200.3, 102.1);
            let second = rect(200.0, 101.5, 300.0, 102.1);
            for rules in [&[(rule, black)][..], &[(first, black), (second, black)]] {
                let f = ruled_found(1, rules, links);
                assert!(f.kept_underlines.contains(&0), "{rules:?}");
                assert_eq!(f.paragraphs[0].status, Status::Translate);
                assert_eq!(paragraph_objects(&f, 0), [1]);
            }
            // Some of its words; two pieces of two colors; a second rule along the line; a
            // line of a paragraph of two: the paragraph stays as it is drawn, rule and all.
            for (lines, rules) in [
                (1, vec![(rect(150.0, 101.5, 250.0, 102.1), black)]),
                (1, vec![(first, black), (second, [0, 0, 200, 255])]),
                (
                    1,
                    vec![(rule, black), (rect(100.0, 103.0, 300.0, 103.4), black)],
                ),
                (2, vec![(rule, black)]),
            ] {
                let f = ruled_found(lines, &rules, links);
                assert!(f.kept_underlines.is_empty(), "{rules:?}");
                assert_eq!(f.paragraphs[0].status, Status::Retain(Retain::Decoration));
            }
        }
        // A rule not in the band of the line's text (8 pt below the baseline) is no matter.
        let f = ruled_found(1, &[(rect(100.0, 108.0, 300.0, 108.6), black)], &[word]);
        assert_eq!(f.paragraphs[0].status, Status::Translate);
        assert!(f.kept_underlines.is_empty());
    }

    #[test]
    fn image_search_text_requires_every_real_run_to_be_invisible() {
        let mut f = found(&TWO_LINES, vec![]);
        f.scan.pictures.push(Rect {
            left: 0.0,
            right: 600.0,
            top: 0.0,
            bottom: 800.0,
        });
        f.scan.origins[0].invisible = true;
        let f = find(f.scan, 0, 10.0);
        assert_eq!(
            f.paragraphs[0].status,
            Status::Retain(Retain::InvisibleSource)
        );
        let mut scan = f.scan;
        for o in &mut scan.origins {
            o.invisible = true;
        }
        assert_eq!(
            find(scan, 0, 10.0).paragraphs[0].status,
            Status::Skip(Skip::ImageText)
        );
    }

    #[test]
    fn uniform_style_uses_body_characters_not_a_short_colored_prefix() {
        let mut f = found(&TWO_LINES, vec![]);
        f.blocks[0].lines[0].items[0].bold = true;
        f.scan.origins[0].count = 1;
        assert!(!majority_style(&f, 0).1);
        assert!(!uniform_anchor(&f, 0));
    }

    #[test]
    fn only_a_whole_rule_inside_one_line_is_a_decoration() {
        let f = found(&TWO_LINES, vec![]);
        let underline = Rect {
            left: 100.0,
            right: 299.0,
            top: 101.0,
            bottom: 101.5,
        };
        assert_eq!(classify_decoration(&f, &underline, [0, 0, 0, 255]), Some(0));
        assert_eq!(
            classify_decoration(
                &f,
                &Rect {
                    right: 310.0,
                    ..underline
                },
                [0, 0, 0, 255]
            ),
            None
        );
        assert_eq!(
            classify_decoration(
                &f,
                &Rect {
                    top: 117.0,
                    bottom: 117.5,
                    ..underline
                },
                [0, 0, 0, 255]
            ),
            None
        );
        assert_eq!(
            classify_decoration(
                &f,
                &Rect {
                    bottom: 103.0,
                    ..underline
                },
                [0, 0, 0, 255]
            ),
            None
        );
    }

    #[test]
    fn a_paragraph_goes_down_to_what_is_below_it() {
        let mut pieces = TWO_LINES.to_vec();
        pieces.push(("A note well below it, apart.", 100.0, 260.0, 200.0, 10.0));
        let f = found(&pieces, Vec::new());
        assert_eq!(f.blocks.len(), 2);
        // To 1 pt above the note's top (200 − 8).
        assert!(
            (limit_below(&f, 0) - 191.0).abs() < 1e-9,
            "{}",
            limit_below(&f, 0)
        );
    }

    #[test]
    fn a_paragraph_is_not_extended_into_text_that_starts_beside_its_foot() {
        // A larger line whose top (118 − 11.2) is above the paragraph's foot (114), across its
        // width: the paragraph keeps its own box.
        let mut pieces = TWO_LINES.to_vec();
        pieces.push(("Beside", 250.0, 350.0, 118.0, 14.0));
        pieces.push(("A note well below it, apart.", 100.0, 260.0, 200.0, 10.0));
        let f = found(&pieces, Vec::new());
        assert_eq!(f.blocks.len(), 3);
        assert_eq!(limit_below(&f, 0), f.blocks[0].bottom);
        // A drawing that starts above the foot too, but for a box around the paragraph.
        let f = found(
            &TWO_LINES,
            vec![Rect {
                left: 280.0,
                right: 400.0,
                top: 110.0,
                bottom: 300.0,
            }],
        );
        assert_eq!(limit_below(&f, 0), f.blocks[0].bottom);
        // A box around the paragraph, its four sides.
        let side = |left: f64, right: f64, top: f64, bottom: f64| Rect {
            left,
            right,
            top,
            bottom,
        };
        let sides = vec![
            side(90.0, 320.0, 80.0, 80.0),
            side(90.0, 320.0, 150.0, 150.0),
            side(90.0, 90.0, 80.0, 150.0),
            side(320.0, 320.0, 80.0, 150.0),
        ];
        let f = found(&TWO_LINES, sides.clone());
        assert!((limit_below(&f, 0) - 149.0).abs() < 1e-9);
        // With a line across it below the paragraph (a table's cells): down to the line.
        let mut cells = sides;
        cells.push(side(90.0, 320.0, 130.0, 130.0));
        let f = found(&TWO_LINES, cells);
        assert!((limit_below(&f, 0) - 129.0).abs() < 1e-9);
        // An image (or anything drawn whole) under the paragraph keeps it to its box.
        let f = found(&TWO_LINES, vec![side(90.0, 320.0, 80.0, 150.0)]);
        assert_eq!(limit_below(&f, 0), f.blocks[0].bottom);
    }

    #[test]
    fn a_translation_over_what_is_in_no_paragraph_is_a_crossing() {
        let mut a = outcome("a", Status::Translate, rect(0.0, 20.0), &["x"]);
        a.drawn = Some(rect(0.0, 40.0));
        // A rule across the page at 30, and one at 10 that the source already ran over.
        let rule = |y: f64| Rect {
            left: 50.0,
            right: 350.0,
            top: y,
            bottom: y,
        };
        assert_eq!(crossings(&[&a], &[rule(30.0)]), ["a"]);
        assert!(crossings(&[&a], &[rule(10.0), rule(45.0)]).is_empty());
        // Down to 1 pt above it, and half a point more, is not over it.
        a.drawn = Some(rect(0.0, 29.5));
        assert!(crossings(&[&a], &[rule(30.0)]).is_empty());
        // An image under the source, which the translation goes further down over.
        let image = Rect {
            left: 50.0,
            right: 350.0,
            top: 5.0,
            bottom: 60.0,
        };
        a.drawn = Some(rect(0.0, 40.0));
        assert_eq!(crossings(&[&a], &[image]), ["a"]);
        a.drawn = Some(rect(0.0, 20.0));
        assert!(crossings(&[&a], &[image]).is_empty());
    }

    #[test]
    fn ruled_cells_are_retained_but_wide_boxed_prose_is_not_a_table() {
        let row = found(
            &[
                ("Results", 100.0, 140.0, 100.0, 10.0),
                ("1234", 170.0, 195.0, 100.0, 10.0),
                ("5678", 230.0, 255.0, 100.0, 10.0),
            ],
            vec![
                Rect {
                    left: 90.0,
                    right: 90.5,
                    top: 80.0,
                    bottom: 110.0,
                },
                Rect {
                    left: 150.0,
                    right: 150.5,
                    top: 80.0,
                    bottom: 110.0,
                },
            ],
        );
        assert_eq!(row.paragraphs.len(), 3);
        assert!(row.paragraphs[0].table);
        assert_eq!(row.paragraphs[0].status, Status::Retain(Retain::Table));
        assert!(row.paragraphs[0].translation_candidate());
        let prose = found(
            &[
                (
                    "A continuous prose sentence containing the year 2026.",
                    100.0,
                    400.0,
                    100.0,
                    10.0,
                ),
                (
                    "Another prose sentence referring to the year 2027.",
                    100.0,
                    390.0,
                    112.0,
                    10.0,
                ),
            ],
            vec![
                Rect {
                    left: 90.0,
                    right: 410.0,
                    top: 85.0,
                    bottom: 85.5,
                },
                Rect {
                    left: 90.0,
                    right: 410.0,
                    top: 120.0,
                    bottom: 120.5,
                },
            ],
        );
        assert_eq!(prose.paragraphs.len(), 1);
        assert!(!prose.paragraphs[0].table);
        assert!(prose.paragraphs[0].translation_candidate());
    }

    #[test]
    fn bullet_slides_are_not_contents_but_page_number_rows_are() {
        for text in ["(1) Main section", "Short independent item"] {
            let f = found(
                &[(text, 100., 270., 100., 10.), (text, 100., 240., 112., 10.)],
                vec![],
            );
            assert!(f.paragraphs.iter().all(|p| !p.table));
        }
        let f = found(
            &[
                ("First chapter ... 1", 100., 270., 100., 10.),
                ("Next chapter ... 2", 100., 270., 112., 10.),
            ],
            vec![],
        );
        assert!(f.paragraphs.iter().any(|p| p.table));
    }

    #[test]
    fn labels_and_prose_depend_only_on_text_shape() {
        for (text, size, expected) in [
            ("Chart legend", 10., true),
            ("Note: small explanatory note.", 7.9, false),
            ("Note: small explanatory note.", 8., false),
            ("A short sentence.", 10., false),
        ] {
            let f = found(&[(text, 100., 300., 100., size)], vec![]);
            assert_eq!(label_block(&f.blocks[0]), expected);
        }
        for n in [80, 81] {
            let text = "x".repeat(n);
            let f = found(&[(&text, 100., 500., 100., 10.)], vec![]);
            assert_eq!(label_block(&f.blocks[0]), n == 80);
            assert!(!prose_block(&f.blocks[0]));
        }
    }

    #[test]
    fn prose_is_not_exempted_by_nearby_artwork() {
        let mut f = found(&TWO_LINES, vec![]);
        f.scan.top_level.clear();
        f.scan.forms = vec![FormInfo::default()];
        f.scan.forms[0].pictures.push(Rect {
            left: 0.,
            right: 600.,
            top: 0.,
            bottom: 800.,
        });
        assert!(!label_block(&f.blocks[0]));
    }

    #[test]
    fn contents_gap_ignores_synthetic_whitespace_and_wrapped_caption_length() {
        let f = found(
            &[
                ("A wrapped description continues", 100., 350., 100., 10.),
                ("on another ordinary line", 100., 350., 112., 10.),
                ("and ends here", 100., 250., 124., 10.),
                (" ", 250., 450., 124., 10.),
                ("15", 450., 460., 124., 10.),
            ],
            vec![],
        );
        assert!(f.paragraphs.iter().any(|p| p.table));
    }

    #[test]
    fn contents_need_leaders_or_gaps() {
        for lines in [
            [
                ("First chapter ... 1", 100., 270., 100., 10.),
                ("Another chapter ... 2", 100., 270., 112., 10.),
            ],
            [
                ("First chapter ... 1", 100., 270., 100., 10.),
                ("Another chapter ... 2", 100., 300., 112., 10.),
            ],
        ] {
            assert!(found(&lines, vec![]).paragraphs.iter().any(|p| p.table));
        }
        let prose = found(
            &[
                ("The first release was in 2025", 100., 270., 100., 10.),
                (
                    "and the next release will follow in 2026",
                    100.,
                    300.,
                    112.,
                    10.,
                ),
            ],
            vec![],
        );
        assert!(prose.paragraphs.iter().all(|p| !p.table));
    }

    #[test]
    fn unplaceable_titles_and_tables_remain_translation_candidates() {
        let mut p = outcome(
            "title",
            Status::Retain(Retain::Overflow),
            rect(0.0, 20.0),
            &["A large title"],
        )
        .paragraph;
        assert!(p.translation_candidate());
        p.table = true;
        p.status = Status::Retain(Retain::Table);
        assert!(p.translation_candidate());
        p.status = Status::Skip(Skip::ImageText);
        assert!(p.translation_candidate());
        p.maths.push(p.rect);
        assert!(p.translation_candidate());
        p.maths.clear();
        p.status = Status::Skip(Skip::NotWorth);
        assert!(!p.translation_candidate());
    }

    fn outcome(id: &str, status: Status, rect: Rect, lines: &[&str]) -> Outcome {
        Outcome {
            source_style: ("10.0".into(), false, [0, 0, 0, 255]),
            paragraph: Paragraph {
                id: id.into(),
                page: 0,
                rect,
                size: 10.0,
                line_height: 1.2,
                align: Align::Left,
                lines: lines.len(),
                text: lines.join(" "),
                body: true,
                table: false,
                prose: true,
                figure: false,
                status: Status::Translate,
                source_lines: lines.iter().map(|l| (*l).to_owned()).collect(),
                maths: Vec::new(),
                line_rects: Vec::new(),
            },
            status,
            scale: None,
            fit_stage: None,
            line_height: None,
            attempts: Vec::new(),
            drawn_lines: 0,
            line_rects: Vec::new(),
            drawn: None,
            wrapped: false,
            translation: None,
        }
    }

    fn rect(top: f64, bottom: f64) -> Rect {
        Rect {
            left: 100.0,
            right: 300.0,
            top,
            bottom,
        }
    }

    #[test]
    fn a_line_twice_on_a_page_must_go_twice() {
        let mut a = outcome("a", Status::Translate, rect(0.0, 10.0), &["Summary"]);
        a.translation = Some("概要".into());
        let mut b = outcome("b", Status::Translate, rect(50.0, 60.0), &["Summary"]);
        b.translation = Some("要約".into());
        let both = [&a, &b];
        // Only one of the two removed.
        let mut t = TextCheck::default();
        check_text("SummaryxSummary", "概要xSummary要約", &both, &mut t);
        assert_eq!(t.remaining, ["a", "b"]);
        let mut t = TextCheck::default();
        check_text("SummaryxSummary", "概要x要約", &both, &mut t);
        assert!(t.remaining.is_empty() && t.missing.is_empty(), "{t:?}");
    }

    #[test]
    fn a_paragraph_kept_must_still_be_read() {
        let mut a = outcome("a", Status::Translate, rect(0.0, 10.0), &["Hello"]);
        a.translation = Some("こんにちは".into());
        let kept = outcome(
            "k",
            Status::Retain(Retain::Overflow),
            rect(50.0, 60.0),
            &["Kepttext", "Hello"],
        );
        let both = [&a, &kept];
        let mut t = TextCheck::default();
        check_text(
            "HelloKepttextHello",
            "こんにちはKepttextHello",
            &both,
            &mut t,
        );
        assert!(t.lost.is_empty(), "{t:?}");
        let mut t = TextCheck::default();
        check_text("HelloKepttextHello", "こんにちはKepttext", &both, &mut t);
        assert_eq!(t.lost, ["k"]);
        // A line kept that is also two lines of a paragraph translated run together.
        let mut b = outcome(
            "b",
            Status::Translate,
            rect(80.0, 100.0),
            &["averageDev", "Set"],
        );
        b.translation = Some("平均".into());
        let dev = outcome(
            "d",
            Status::Skip(Skip::NotWorth),
            rect(50.0, 60.0),
            &["DevSet"],
        );
        let mut t = TextCheck::default();
        check_text("DevSetaverageDevSet", "DevSet平均", &[&b, &dev], &mut t);
        assert!(t.lost.is_empty() && t.remaining.is_empty(), "{t:?}");
        let mut t = TextCheck::default();
        check_text("DevSetaverageDevSet", "平均", &[&b, &dev], &mut t);
        assert_eq!(t.lost, ["d"]);
        // One read across two paragraphs, one of them translated.
        let mut c = outcome("c", Status::Translate, rect(80.0, 100.0), &["2025年度実績"]);
        c.translation = Some("FY2025".into());
        let n62 = outcome("n", Status::Skip(Skip::NotWorth), rect(50.0, 60.0), &["62"]);
        let n86 = outcome("m", Status::Skip(Skip::NotWorth), rect(60.0, 70.0), &["86"]);
        let all = [&c, &n62, &n86];
        let mut t = TextCheck::default();
        check_text("62x862025年度実績", "62x86FY2025", &all, &mut t);
        assert!(t.lost.is_empty(), "{t:?}");
        let mut t = TextCheck::default();
        check_text("62x862025年度実績", "x86FY2025", &all, &mut t);
        assert_eq!(t.lost, ["n"]);
    }

    #[test]
    fn a_line_kept_is_counted_where_it_is_not_inside_another() {
        // The raw extraction also finds translated `Hello` inside retained `XHelloY`. The audit must
        // fail if a write removes both lines.
        let mut a = outcome("a", Status::Translate, rect(50.0, 60.0), &["Hello"]);
        a.translation = Some("Translated".into());
        let kept = outcome(
            "k",
            Status::Retain(Retain::Overflow),
            rect(0.0, 10.0),
            &["XHelloY"],
        );
        let both = [&a, &kept];
        let mut t = TextCheck::default();
        check_text("XHelloYHello", "Translated", &both, &mut t);
        assert_eq!(t.lost, ["k"], "{t:?}");
        let mut t = TextCheck::default();
        check_text("XHelloYHello", "XHelloYTranslated", &both, &mut t);
        assert!(
            t.lost.is_empty() && t.remaining.is_empty() && t.missing.is_empty(),
            "{t:?}"
        );
        // A line found only inside another paragraph's: where it is cannot be told.
        let inner = outcome(
            "i",
            Status::Retain(Retain::Overflow),
            rect(20.0, 30.0),
            &["ell"],
        );
        let mut t = TextCheck::default();
        check_text("XHelloY", "XHelloY", &[&kept, &inner], &mut t);
        assert_eq!(t.unplaced, ["i"], "{t:?}");
        // A line found once is placed first: `6MOSS` (a heading further on) is also read across
        // the page number `8256` and the `MOSS` after it.
        let page = outcome(
            "n",
            Status::Skip(Skip::NotWorth),
            rect(0.0, 10.0),
            &["8256"],
        );
        let mut heading = outcome("h", Status::Translate, rect(50.0, 60.0), &["6MOSS"]);
        heading.translation = Some("6 MOSS".into());
        let mut t = TextCheck::default();
        check_text(
            "8256MOSSx6MOSS",
            "8256MOSSx6MOSS",
            &[&page, &heading],
            &mut t,
        );
        assert!(t.unplaced.is_empty(), "{t:?}");
    }

    #[test]
    fn two_translations_alike_must_both_be_read() {
        let mut a = outcome("a", Status::Translate, rect(0.0, 10.0), &["first"]);
        a.translation = Some("SameTranslation".into());
        let mut b = outcome("b", Status::Translate, rect(50.0, 60.0), &["second"]);
        b.translation = Some("SameTranslation".into());
        let mut t = TextCheck::default();
        check_text("firstsecond", "SameTranslation", &[&a, &b], &mut t);
        assert_eq!(t.missing, ["a", "b"], "{t:?}");
        let mut t = TextCheck::default();
        check_text(
            "firstsecond",
            "SameTranslationSameTranslation",
            &[&a, &b],
            &mut t,
        );
        assert!(t.missing.is_empty(), "{t:?}");
        // One inside another, and one that the text kept has too.
        b.translation = Some("SameTranslation, again".into());
        let mut t = TextCheck::default();
        check_text("firstsecond", "SameTranslation,again", &[&a, &b], &mut t);
        assert_eq!(t.missing, ["a"], "{t:?}");
        let mut c = outcome("c", Status::Translate, rect(80.0, 90.0), &["third"]);
        c.translation = Some("Note".into());
        let kept = outcome(
            "k",
            Status::Skip(Skip::NotWorth),
            rect(20.0, 30.0),
            &["Note"],
        );
        let mut t = TextCheck::default();
        check_text("Notethird", "Note", &[&c, &kept], &mut t);
        assert_eq!(t.missing, ["c"], "{t:?}");
    }

    #[test]
    fn a_translation_over_another_paragraph_is_an_overlap() {
        let mut a = outcome("a", Status::Translate, rect(0.0, 20.0), &["x"]);
        a.drawn = Some(rect(0.0, 40.0));
        let kept = outcome(
            "k",
            Status::Retain(Retain::Overflow),
            rect(30.0, 50.0),
            &["y"],
        );
        assert_eq!(overlaps(&[&a, &kept]), [("a".into(), "k".into())]);
        // Boxes that overlapped already in the source, no more than they did (but for the
        // half point a translation may go below its box).
        a.drawn = Some(rect(0.0, 20.5));
        let close = outcome(
            "c",
            Status::Retain(Retain::Overflow),
            rect(15.0, 35.0),
            &["y"],
        );
        assert!(overlaps(&[&a, &close]).is_empty());
    }

    fn texts(a: &[Atom]) -> Vec<&str> {
        a.iter().map(|a| a.text.as_str()).collect()
    }

    #[test]
    fn closing_angle_marks_do_not_break_after_spaces() {
        let a = atoms("＜Overview ＞");
        assert_eq!(a.len(), 1);
        assert_eq!(a[0].text, "＜Overview ＞");
        let a = atoms("< Overview >");
        assert_eq!(a.len(), 1);
        assert_eq!(a[0].text, "< Overview >");
    }

    #[test]
    fn english_breaks_between_words_only() {
        let a = atoms("The model, trained (twice).");
        assert_eq!(texts(&a), ["The", "model,", "trained", "(twice)."]);
        assert!(!a[0].space && a[1].space);
    }

    #[test]
    fn japanese_breaks_between_characters_but_keeps_kinsoku() {
        let a = atoms("模型部員は「装飾」を見る。");
        assert_eq!(
            texts(&a),
            [
                "模", "型", "部", "員", "は", "「装", "飾」", "を", "見", "る。"
            ]
        );
        // Small kana stay with what comes before them.
        assert_eq!(texts(&atoms("ショップ")), ["ショッ", "プ"]);
        assert_eq!(texts(&atoms("で2026年")), ["で", "2026", "年"]);
    }

    #[test]
    fn a_latin_word_inside_japanese_is_kept_whole() {
        let a = atoms("PDFiumで翻訳");
        assert_eq!(texts(&a), ["PDFium", "で", "翻", "訳"]);
    }
}
