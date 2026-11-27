//! Underlines, and the rules that are not underlines (the sides of boxes, the rules of tables,
//! separators, lines to write on): shapes built here and run through the file tab's pipeline
//! ([`Document::write`], page fallback included).
//!
//! No path of a page is ever taken off, moved or drawn anew: an underline and a table's rule are
//! drawn alike, and no evidence tells them apart (three designs that took some rules for
//! underlines each broke a table's rule set by pdfLaTeX). Every shape here, negative or positive,
//! is held to that ([`a_save_draws_every_path_as_its_source_did`]). Negative shapes: no ink of a
//! box, a table, a separator, a figure, or of an underline may be lost (`keep`). Positive shapes:
//! underlines as Word, LibreOffice, Chrome, pdfTeX and PowerPoint draw them, under a link
//! annotation or not, stay as they are drawn; a line underlined end to end may be set over its
//! rule, and a line underlined in some words stays as it is.
use super::*;
use crate::pdf::export::{Document, PageKind};

/// Test row in Courier 12 pt at 95% width from (72, 740), with descenders (g, p, y).
const ROW: &str = "Gardening group keeps the beds green.";
/// The same without a descender.
const CAPS: &str = "KEEP THE BEDS GREEN AT HOME.";
/// Bottom edge of the row's rule, 1.6 pt below the baseline (0.6 pt thick).
const Y: f64 = 738.4;
const TRANSLATION: &str = "Translated gardening information.";

fn width(row: &str) -> f64 {
    row.len() as f64 * 7.2 * 0.95
}

fn text(s: &str, x: f64, y: f64) -> String {
    format!("BT /F1 12 Tf 0 Tr 95 Tz 1 0 0 1 {x} {y} Tm ({s}) Tj ET\n")
}

/// Text as the positive shapes set it, in its own BT/ET (one Courier character is 6.84 pt at
/// 95%: at 100% its space is 0.6 em, where a gap between pieces of a line starts).
fn plain(s: &str, x: f64, y: f64) -> String {
    format!("BT /F1 12 Tf 95 Tz 1 0 0 1 {x} {y} Tm ({s}) Tj ET\n")
}

#[derive(Clone, Default)]
pub(super) struct Shape {
    pub name: String,
    pub content: String,
    /// Form XObjects, `/Fm0`, `/Fm1`…, each over the whole page.
    pub forms: Vec<String>,
    pub rotate: u32,
    pub annots: Vec<String>,
    /// Page-space rectangles `[left, bottom, right, top]` whose ink must stay.
    pub keep: Vec<[f64; 4]>,
}

fn shape(name: impl Into<String>, content: String, keep: Vec<[f64; 4]>) -> Shape {
    Shape {
        name: name.into(),
        content,
        keep,
        ..Shape::default()
    }
}

/// A link annotation (to a URL, drawing nothing) over the page-space `[left, bottom, right, top]`.
pub(super) fn link_annot([l, b, r, t]: [f64; 4]) -> String {
    format!(
        "<< /Type /Annot /Subtype /Link /Rect [{l} {b} {r} {t}] /Border [0 0 0] \
         /A << /S /URI /URI (https://example.invalid/) >> >>"
    )
}

/// The shape with a link annotation over `rect` too.
pub(super) fn with_link(mut s: Shape, name: &str, rect: [f64; 4]) -> Shape {
    s.name = name.into();
    s.annots.push(link_annot(rect));
    s
}

/// Word's link annotation over text from `x0` to `x1` on the baseline `base`: 2.25 pt past
/// either end, from 2.4 pt below the baseline to 10.1 above, following the line’s text box.
fn word_link(x0: f64, x1: f64, base: f64) -> [f64; 4] {
    [x0 - 2.25, base - 2.4, x1 + 2.2, base + 10.1]
}

/// hyperref's link annotation over a box from `x0` to `x1` on the baseline `base`, as pdfTeX
/// sets it with `\pdflinkmargin` 1 pt: over `\underline` (5.31 below, 9.96 above), or over a
/// table's cell (its strut: 5.33 below, 11.11 above).
fn tex_link(x0: f64, x1: f64, base: f64, cell: bool) -> [f64; 4] {
    if cell {
        [x0 - 0.996, base - 5.33, x1 + 0.996, base + 11.108]
    } else {
        [x0 - 0.996, base - 5.31, x1 + 0.996, base + 9.963]
    }
}

/// An A4 page of the shape: Courier (WinAnsi, not embedded) as `/F1`, a half-transparent
/// graphics state `/Half`, its Forms and annotations.
pub(super) fn pdf(s: &Shape) -> Vec<u8> {
    let first_form = 6;
    let first_annot = first_form + s.forms.len();
    let xobjects: String = (0..s.forms.len())
        .map(|k| format!("/Fm{k} {} 0 R ", first_form + k))
        .collect();
    let annots: String = (0..s.annots.len())
        .map(|k| format!("{} 0 R ", first_annot + k))
        .collect();
    let font = "/Font << /F1 4 0 R >>";
    let mut objects = vec![
        "<< /Type /Catalog /Pages 2 0 R >>".to_owned(),
        "<< /Type /Pages /Kids [3 0 R] /Count 1 >>".to_owned(),
        format!(
            "<< /Type /Page /Parent 2 0 R /MediaBox [0 0 595 842] /Rotate {} /Resources << {font} \
             /XObject << {xobjects}>> /ExtGState << /Half << /Type /ExtGState /ca 0.5 /CA 0.5 >> \
             >> >> /Contents 5 0 R /Annots [{annots}] >>",
            s.rotate
        ),
        format!(
            "<< /Type /Font /Subtype /Type1 /BaseFont /Courier /Encoding /WinAnsiEncoding \
             /FirstChar 32 /LastChar 255 /Widths [{}] >>",
            "600 ".repeat(224)
        ),
        format!(
            "<< /Length {} >>\nstream\n{}\nendstream",
            s.content.len() + 1,
            s.content
        ),
    ];
    for form in &s.forms {
        objects.push(format!(
            "<< /Type /XObject /Subtype /Form /BBox [0 0 595 842] /Resources << {font} >> \
             /Length {} >>\nstream\n{form}\nendstream",
            form.len() + 1
        ));
    }
    objects.extend(s.annots.iter().cloned());
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

/// What the file tab made of the shape's page: its kind (`None` where the save failed), the
/// paragraphs placed on it, and how many pixels of what was to be kept turned lighter (at
/// 144 dpi, by more than 24 levels): on the saved page, and on the placement before the checks
/// of the save (which may send the page back to the original).
pub(super) struct Run {
    pub kind: Option<PageKind>,
    pub placed: Vec<String>,
    pub lost: usize,
    pub lost_placement: usize,
    /// Thin rules along lines on the saved page.
    pub rules: usize,
    /// Over every page of the save, against its source page: the paths not drawn as they were,
    /// and the pixels of the drawing not as they were ([`drawing_changes`]). `None` where the
    /// save failed.
    pub saved: Option<(usize, usize)>,
    /// The same of the placement, before the checks of the save.
    pub placement: (usize, usize),
}

/// How page `b` of `output` draws what page `a` of `source` drew: the paths it does not draw as
/// `source` did ([`paths_changed`], with nothing let pass), and the pixels (144 dpi) that differ
/// at all once the text of both pages is taken off (what is left is paths, images, shadings and
/// Forms: whatever was not to be touched).
pub(super) fn drawing_changes(source: &[u8], output: &[u8], a: usize, b: usize) -> (usize, usize) {
    let (_guard, pdfium) = pdfium().unwrap();
    let lib = pdfium.bindings();
    let (da, db) = (
        Doc::load(lib, source).unwrap(),
        Doc::load(lib, output).unwrap(),
    );
    let (pa, pb) = (da.page(a).unwrap(), db.page(b).unwrap());
    let paths = paths_changed(lib, &pa, &pb);
    let drawing = |page: &Page<'_>| {
        let mut taken = Vec::new();
        let mut k = 0;
        while k < lib.FPDFPage_CountObjects(page.raw) {
            let object = lib.FPDFPage_GetObject(page.raw, k);
            if !object.is_null()
                && lib.FPDFPageObj_GetType(object) == PAGEOBJ_TEXT
                && lib.FPDFPage_RemoveObject(page.raw, object) != 0
            {
                taken.push(object);
            } else {
                k += 1;
            }
        }
        let bitmap = render(lib, page, 2.0);
        for object in taken {
            lib.FPDFPageObj_Destroy(object);
        }
        bitmap
    };
    let ((w, h, x), (w2, h2, y)) = (drawing(&pa), drawing(&pb));
    let pixels = if (w, h) == (w2, h2) {
        x.chunks(4)
            .zip(y.chunks(4))
            .filter(|(p, q)| p[..3] != q[..3])
            .count()
    } else {
        w.max(w2) * h.max(h2)
    };
    (paths, pixels)
}

pub(super) fn translate(s: &Shape) -> Run {
    let source = pdf(s);
    // For looking at the shapes outside the test.
    if let Some(dir) = std::env::var_os("PRIVATE_TRANSLATE_UNDERLINE_SHAPES") {
        std::fs::write(
            std::path::Path::new(&dir).join(format!("{}.pdf", s.name)),
            &source,
        )
        .unwrap();
    }
    let reference = pdf(&drawing(s));
    let file = tempfile::Builder::new().suffix(".pdf").tempfile().unwrap();
    std::fs::write(file.path(), &source).unwrap();
    let doc = Document::read(file.path()).unwrap_or_else(|e| panic!("{}: {e}", s.name));
    let targets: Vec<_> = doc
        .units()
        .iter()
        .map(|_| Some(TRANSLATION.to_owned()))
        .collect();
    let sum = |a: (usize, usize), b: (usize, usize)| (a.0 + b.0, a.1 + b.1);
    match doc.write(&targets, "en") {
        Ok(out) => {
            let page = &out.pages[0];
            let placement = out.placement.as_ref().map(|p| p.bytes.clone());
            Run {
                kind: Some(page.kind),
                placed: page.placed.clone(),
                lost: lost_ink_kept(&reference, &out.bytes, page.output_page - 1, &s.keep),
                lost_placement: placement
                    .as_ref()
                    .map_or(0, |bytes| lost_ink_kept(&reference, bytes, 0, &s.keep)),
                rules: rules_on(&out.bytes, page.output_page - 1),
                saved: Some(out.pages.iter().fold((0, 0), |n, p| {
                    sum(
                        n,
                        drawing_changes(&source, &out.bytes, p.page - 1, p.output_page - 1),
                    )
                })),
                placement: placement.as_ref().map_or((0, 0), |bytes| {
                    out.pages.iter().fold((0, 0), |n, p| {
                        sum(n, drawing_changes(&source, bytes, p.page - 1, p.page - 1))
                    })
                }),
            }
        }
        // A page the save cannot write (as a page turned by `/Rotate` whose text is upright:
        // the bilingual writer fails on it, before and after this work). What the placement
        // would have drawn is still measured.
        Err(_) => {
            let map: HashMap<String, String> = doc
                .paragraphs()
                .iter()
                .map(|p| (p.id.clone(), TRANSLATION.to_owned()))
                .collect();
            let placement = crate::pdf::export::write(&source, None, &map, Options::default())
                .unwrap_or_else(|e| panic!("{}: {e}", s.name));
            Run {
                kind: None,
                placed: Vec::new(),
                lost: 0,
                lost_placement: lost_ink_kept(&reference, &placement.bytes, 0, &s.keep),
                rules: 0,
                saved: None,
                placement: drawing_changes(&source, &placement.bytes, 0, 0),
            }
        }
    }
}

/// The shape without its text: what is drawn under and around the text, as it is to stay.
pub(super) fn drawing(s: &Shape) -> Shape {
    let mut content = String::new();
    let mut rest = s.content.as_str();
    while let Some(start) = rest.find("BT ") {
        content.push_str(&rest[..start]);
        let end = rest[start..].find(" ET").expect("ET") + start + 3;
        rest = &rest[end..];
    }
    content.push_str(rest);
    Shape {
        content,
        ..s.clone()
    }
}

/// The pixels inside `keep` (page space of the first page of `reference`, the shape's drawing
/// without its text) lighter on `page` of `output`, by more than 24 levels in a color, at
/// 144 dpi. The text's own glyphs over a rule are not counted: they were the text's.
pub(super) fn lost_ink_kept(
    reference: &[u8],
    output: &[u8],
    page: usize,
    keep: &[[f64; 4]],
) -> usize {
    let (_guard, pdfium) = pdfium().unwrap();
    let a = Doc::load(pdfium.bindings(), reference).unwrap();
    let b = Doc::load(pdfium.bindings(), output).unwrap();
    let (pa, pb) = (a.page(0).unwrap(), b.page(page).unwrap());
    let frame = Frame::of(&pa);
    let scale = 2.0;
    let (w, h, ra) = render(a.lib, &pa, scale);
    let (w2, h2, rb) = render(b.lib, &pb, scale);
    assert_eq!((w, h), (w2, h2));
    let mut lost = 0;
    for [l, bottom, r, t] in keep {
        let rect = frame.rect(*l, *bottom, *r, *t);
        let x0 = ((rect.left * scale).floor().max(0.0) as usize).min(w);
        let x1 = ((rect.right * scale).ceil().max(0.0) as usize).min(w);
        let y0 = ((rect.top * scale).floor().max(0.0) as usize).min(h);
        let y1 = ((rect.bottom * scale).ceil().max(0.0) as usize).min(h);
        for y in y0..y1 {
            for x in x0..x1 {
                let k = (y * w + x) * 4;
                if (0..3).any(|c| rb[k + c] > ra[k + c].saturating_add(24)) {
                    lost += 1;
                }
            }
        }
    }
    lost
}

/// The shape on a page turned by `/Rotate` (90, 180, 270), drawn so that it is upright as the
/// page is shown: its content in a space that undoes the turn (moved down 200 pt to fit the
/// 595 pt height of a page turned a quarter), its `keep` taken there too.
pub(super) fn turned(s: Shape, rotate: u32) -> Shape {
    // From the upright space of the shown page to page space.
    let m: [f64; 6] = match rotate {
        90 => [0.0, 1.0, -1.0, 0.0, 595.0, 0.0],
        180 => [-1.0, 0.0, 0.0, -1.0, 595.0, 842.0],
        _ => [0.0, -1.0, 1.0, 0.0, 0.0, 842.0],
    };
    let shift = if rotate == 180 { 0.0 } else { -200.0 };
    let at = |x: f64, y: f64| {
        let y = y + shift;
        (m[0] * x + m[2] * y + m[4], m[1] * x + m[3] * y + m[5])
    };
    let keep = s
        .keep
        .iter()
        .map(|[l, b, r, t]| {
            let (x1, y1) = at(*l, *b);
            let (x2, y2) = at(*r, *t);
            [x1.min(x2), y1.min(y2), x1.max(x2), y1.max(y2)]
        })
        .collect();
    let cm = format!(
        "q {} {} {} {} {} {} cm 1 0 0 1 0 {shift} cm\n",
        m[0], m[1], m[2], m[3], m[4], m[5]
    );
    Shape {
        name: format!("{}-rotated-{rotate}", s.name),
        content: format!("{cm}{}Q\n", s.content),
        rotate,
        keep,
        ..s
    }
}

// ---- Negative shapes ----

/// A gapped frame: the row's rule (`Y`) is the top of a box whose sides
/// and bottom are other paths, stopping `gap` below it: `solid`, `dash`, `dot`, `fill` (thin
/// filled rectangles), `clip` (sides clipped there), `form` (sides in a Form).
pub(super) fn gap_frame(row: &str, gap: f64, style: &str) -> Shape {
    let w = width(row);
    let (r, lo) = (72.0 + w, Y - gap);
    let strokes =
        format!("0 G 0.4 w 72 {lo} m 72 720 l S {r} {lo} m {r} 720 l S 72 720 m {r} 720 l S\n");
    let sides = match style {
        "dash" => format!("[2 2] 0 d {strokes}"),
        "dot" => format!("[0.2 2] 0 d 1 J {strokes}"),
        "fill" => format!(
            "72 720 0.4 {h} re f {} 720 0.4 {h} re f 72 720 {w} 0.4 re f\n",
            r - 0.4,
            h = lo - 720.0
        ),
        "clip" => format!("q 70 719 {} {} re W n {strokes}Q\n", w + 4.0, lo - 719.0),
        _ => strokes,
    };
    let top = format!("72 {Y} {w} 0.6 re f\n");
    let keep = if style == "fill" {
        vec![
            [72.0, Y, r, Y + 0.6],
            [72.0, 720.0, 72.4, lo],
            [r - 0.4, 720.0, r, lo],
            [72.0, 720.0, r, 720.4],
        ]
    } else {
        vec![
            [72.0, Y, r, Y + 0.6],
            [71.8, 720.0, 72.2, lo],
            [r - 0.2, 720.0, r + 0.2, lo],
            [72.0, 719.8, r, 720.2],
        ]
    };
    let name = format!("gap-{gap}-{style}{}", if row == ROW { "" } else { "-caps" });
    if style == "form" {
        return Shape {
            forms: vec![sides],
            ..shape(
                name,
                format!("{}{top}/Fm0 Do\n", text(row, 72.0, 740.0)),
                keep,
            )
        };
    }
    shape(
        name,
        format!("{}{top}{sides}", text(row, 72.0, 740.0)),
        keep,
    )
}

/// A rule `offset` below the baseline, a
/// cell's sides going down 15 pt from it, and (closed) the bottom.
pub(super) fn cell_below(offset: f64, closed: bool) -> Shape {
    let w = width(ROW);
    let (r, high, low) = (72.0 + w, 740.0 - offset, 725.0 - offset);
    let mut content = format!(
        "{}0 g 72 {high} {w} 0.6 re f\n0 G 0.4 w 72 {low} m 72 {high} l S {r} {low} m {r} {high} l S\n",
        text(ROW, 72.0, 740.0)
    );
    let mut keep = vec![
        [72.0, high, r, high + 0.6],
        [71.8, low, 72.2, high],
        [r - 0.2, low, r + 0.2, high],
    ];
    if closed {
        content.push_str(&format!("72 {low} m {r} {low} l S\n"));
        keep.push([72.0, low - 0.2, r, low + 0.2]);
    }
    let name = format!(
        "{}-{offset}",
        if closed {
            "closed-below"
        } else {
            "cell95-below"
        }
    );
    shape(name, content, keep)
}

/// Open cells, closed cells and gapped frames whose borders must be preserved.
pub(super) fn cell_and_frame_shapes() -> Vec<Shape> {
    let mut out = Vec::new();
    for offset in [1.2, 1.6] {
        out.push(cell_below(offset, false));
        out.push(cell_below(offset, true));
    }
    for (gap, styles) in [
        (1.0, &["clip", "dash", "form", "solid"][..]),
        (1.2, &["clip", "dash", "dot", "form", "solid"][..]),
        (1.5, &["clip", "dash", "dot", "fill", "form", "solid"][..]),
        (1.8, &["clip", "dash", "dot", "fill", "form", "solid"][..]),
        (2.0, &["clip", "dash", "dot", "fill", "form", "solid"][..]),
    ] {
        for style in styles {
            out.push(gap_frame(ROW, gap, style));
        }
    }
    out
}

/// A rule across the row at `Y` (0.6 pt), and what else is drawn.
fn ruled(name: &str, row: &str, extra: &str, mut keep: Vec<[f64; 4]>) -> Shape {
    let r = 72.0 + width(row);
    keep.push([72.0, Y, r, Y + 0.6]);
    shape(
        name,
        format!(
            "{}0 g 72 {Y} {} 0.6 re f\n{extra}",
            text(row, 72.0, 740.0),
            width(row)
        ),
        keep,
    )
}

/// Shapes around what an underline looks like that are not one (or not only one).
pub(super) fn more_shapes() -> Vec<Shape> {
    let mut out = Vec::new();
    let w = width(ROW);
    let r = 72.0 + w;
    let wc = width(CAPS);
    let rc = 72.0 + wc;
    // Wider and narrower corner gaps, with and without descenders touching the top.
    for gap in [0.5, 3.0, 5.0, 8.0] {
        out.push(gap_frame(ROW, gap, "solid"));
    }
    for gap in [0.5, 1.0, 2.0, 3.0, 5.0, 8.0] {
        out.push(gap_frame(CAPS, gap, "solid"));
    }
    out.push(gap_frame(CAPS, 1.0, "dash"));
    out.push(gap_frame(CAPS, 1.0, "form"));
    // A box with rounded corners: one path; and its top as a path of its own between corner
    // curves drawn apart.
    out.push(shape(
        "rounded",
        format!(
            "{}0 G 0.6 w 76 {Y} m {a} {Y} l {r} {Y} {r} {b} {r} {b} c {r} 724 l {r} 720 {a} 720 {a} 720 c \
             76 720 l 72 720 72 724 72 724 c 72 {b} l 72 {Y} 76 {Y} 76 {Y} c h S\n",
            text(ROW, 72.0, 740.0),
            a = r - 4.0,
            b = Y - 4.0
        ),
        vec![[76.0, Y - 0.3, r - 4.0, Y + 0.3], [71.7, 724.0, 72.3, Y - 4.0]],
    ));
    out.push(ruled(
        "rounded-parts",
        CAPS,
        &format!(
            "0 G 0.4 w 72 {b} m 72 {Y} 72 {Y} 74 {Y} c S {rc} {b} m {rc} {Y} {rc} {Y} {d} {Y} c S \
             72 {b} m 72 720 l S {rc} {b} m {rc} 720 l S 72 720 m {rc} 720 l S\n",
            b = Y - 2.0,
            d = rc - 2.0
        ),
        vec![
            [71.8, 720.0, 72.2, Y - 2.0],
            [rc - 0.2, 720.0, rc + 0.2, Y - 2.0],
        ],
    ));
    // L: the top and the left side only; with and without a gap.
    for (name, gap) in [("ell", 0.0), ("ell-gap-1", 1.0)] {
        out.push(ruled(
            name,
            CAPS,
            &format!("0 G 0.4 w 72 {lo} m 72 700 l S\n", lo = Y - gap),
            vec![[71.8, 700.0, 72.2, Y - gap]],
        ));
    }
    // A cell above the rule (the rule its bottom), closed; touching and with a gap.
    for (name, gap) in [("cup-up", 0.0), ("cup-up-gap-2", 2.0)] {
        out.push(ruled(
            name,
            CAPS,
            &format!(
                "0 G 0.4 w 72 {lo} m 72 760 l S {rc} {lo} m {rc} 760 l S 72 760 m {rc} 760 l S\n",
                lo = Y + 0.6 + gap
            ),
            vec![
                [71.8, Y + 0.6 + gap, 72.2, 760.0],
                [rc - 0.2, Y + 0.6 + gap, rc + 0.2, 760.0],
                [72.0, 759.8, rc, 760.2],
            ],
        ));
    }
    // Table cells: the cell's bottom border touching the descenders, and clear of them, with a
    // cell to its right.
    for (name, offset) in [("cell-touch", 1.6), ("cell-clear", 4.0)] {
        let b = 740.0 - offset;
        out.push(shape(
            name,
            format!(
                "{}{}0 G 0.5 w 70 {b} m {e} {b} l S 70 756 m {e} 756 l S 70 {b} m 70 756 l S \
                 {m} {b} m {m} 756 l S {e} {b} m {e} 756 l S\n",
                text(ROW, 72.0, 740.0),
                text("Spring", r + 6.0, 740.0),
                m = r + 2.0,
                e = r + 60.0
            ),
            vec![
                [70.0, b - 0.25, r + 60.0, b + 0.25],
                [69.75, b, 70.25, 756.0],
                [r + 1.75, b, r + 2.25, 756.0],
            ],
        ));
    }
    // A table of horizontal rules only: one column, the rows' text as wide as the rules.
    for (name, in_flow) in [("hrules-in-flow", true), ("hrules-after", false)] {
        let rules = [Y, Y - 16.0, Y + 14.0];
        let row2 = "Water the seedlings every day.";
        let w2 = width(row2);
        let line = |y: f64, w: f64| format!("0 g 72 {y} {w} 0.6 re f\n");
        let content = if in_flow {
            format!(
                "{}{}{}{}",
                line(rules[2], w),
                text(ROW, 72.0, 740.0) + &line(rules[0], w),
                text(row2, 72.0, 724.0),
                line(rules[1], w2)
            )
        } else {
            format!(
                "{}{}{}{}{}",
                text(ROW, 72.0, 740.0),
                text(row2, 72.0, 724.0),
                line(rules[2], w),
                line(rules[0], w),
                line(rules[1], w2)
            )
        };
        out.push(shape(
            name,
            content,
            vec![
                [72.0, rules[2], r, rules[2] + 0.6],
                [72.0, rules[0], r, rules[0] + 0.6],
                [72.0, rules[1], 72.0 + w2, rules[1] + 0.6],
            ],
        ));
    }
    // A table of vertical rules only, 2 pt from an underlined cell's text.
    out.push(ruled(
        "vrules",
        ROW,
        &format!(
            "0 G 0.4 w 70 700 m 70 760 l S {} 700 m {} 760 l S\n",
            r + 2.0,
            r + 2.0
        ),
        vec![[69.8, 700.0, 70.2, 760.0], [r + 1.8, 700.0, r + 2.2, 760.0]],
    ));
    // An underline whose end meets a table's rule going on, or a box's side going down.
    out.push(ruled(
        "meets-rule",
        CAPS,
        &format!("0 g {rc} {Y} 120 0.6 re f\n"),
        vec![[rc, Y, rc + 120.0, Y + 0.6]],
    ));
    out.push(ruled(
        "meets-side",
        CAPS,
        &format!("0 G 0.4 w {rc} {Y} m {rc} 700 l S\n"),
        vec![[rc - 0.2, 700.0, rc + 0.2, Y]],
    ));
    // One path of two horizontal lines (the row's and another).
    out.push(shape(
        "two-in-one-path",
        format!(
            "{}0 G 0.6 w 72 {y} m {rc} {y} l 72 700 m {rc} 700 l S\n",
            text(CAPS, 72.0, 740.0),
            y = Y + 0.3
        ),
        vec![[72.0, Y, rc, Y + 0.6], [72.0, 699.7, rc, 700.3]],
    ));
    // The underline drawn in two pieces, the second going into a box's side.
    out.push(shape(
        "pieces-into-side",
        format!(
            "{}0 g 72 {Y} {h} 0.6 re f {m} {Y} {h} 0.6 re f\n0 G 0.4 w {rc} {Y} m {rc} 700 l S \
             72 700 m {rc} 700 l S\n",
            text(CAPS, 72.0, 740.0),
            h = wc / 2.0,
            m = 72.0 + wc / 2.0
        ),
        vec![
            [72.0, Y, rc, Y + 0.6],
            [rc - 0.2, 700.0, rc + 0.2, Y],
            [72.0, 699.8, rc, 700.2],
        ],
    ));
    // A table drawn as thin filled rectangles (as Chrome draws borders), the cell's bottom as
    // wide as its text.
    out.push(shape(
        "filled-cells",
        format!(
            "0 g 71.4 {Y} 0.6 22 re f {rc} {Y} 0.6 22 re f 72 {t} {wc} 0.6 re f 72 {Y} {wc} 0.6 re f\n{}",
            text(CAPS, 72.0, 740.0),
            t = Y + 21.4
        ),
        vec![
            [71.4, Y, 72.0, Y + 22.0],
            [rc, Y, rc + 0.6, Y + 22.0],
            [72.0, Y, rc, Y + 0.6],
        ],
    ));
    // The rule itself (and the box) in a Form.
    out.push(Shape {
        forms: vec![format!(
            "0 g 72 {Y} {wc} 0.6 re f 0 G 0.4 w 72 {lo} m 72 720 l S {rc} {lo} m {rc} 720 l S\n",
            lo = Y - 1.0
        )],
        ..shape(
            "rule-in-form",
            format!("{}/Fm0 Do\n", text(CAPS, 72.0, 740.0)),
            vec![[72.0, Y, rc, Y + 0.6], [71.8, 720.0, 72.2, Y - 1.0]],
        )
    });
    // A long rule clipped to the row's width; and the row's rule clipped to half of it.
    out.push(shape(
        "clip-long",
        format!(
            "{}q 72 700 {wc} 60 re W n 0 g 40 {Y} 500 0.6 re f Q\n",
            text(CAPS, 72.0, 740.0)
        ),
        vec![[72.0, Y, rc, Y + 0.6]],
    ));
    out.push(shape(
        "clip-half",
        format!(
            "{}q 72 700 {h} 60 re W n 0 g 72 {Y} {wc} 0.6 re f Q\n",
            text(CAPS, 72.0, 740.0),
            h = wc / 2.0
        ),
        vec![[72.0, Y, 72.0 + wc / 2.0, Y + 0.6]],
    ));
    // Turned pages (the text upright as the page is shown), and boxes drawn in a turned or
    // scaled space.
    for rotate in [90, 180, 270] {
        out.push(turned(gap_frame(CAPS, 1.0, "solid"), rotate));
        out.push(turned(gap_frame(ROW, 1.0, "solid"), rotate));
    }
    {
        let base = gap_frame(CAPS, 1.0, "solid");
        // Every page-space y as 842 - y in the flipped space.
        out.push(Shape {
            name: "flipped-cm".into(),
            content: format!(
                "{}q 1 0 0 -1 0 842 cm 0 g 72 {} {wc} -0.6 re f 0 G 0.4 w 72 {} m 72 122 l S \
                 {rc} {} m {rc} 122 l S 72 122 m {rc} 122 l S Q\n",
                text(CAPS, 72.0, 740.0),
                842.0 - Y,
                842.0 - (Y - 1.0),
                842.0 - (Y - 1.0)
            ),
            ..base.clone()
        });
        out.push(Shape {
            name: "scaled-cm".into(),
            content: format!(
                "{}q 0.5 0 0 0.5 0 0 cm 0 g 144 {} {} 1.2 re f 0 G 0.8 w 144 {} m 144 1440 l S \
                 {r2} {} m {r2} 1440 l S 144 1440 m {r2} 1440 l S Q\n",
                text(CAPS, 72.0, 740.0),
                2.0 * Y,
                2.0 * wc,
                2.0 * (Y - 1.0),
                2.0 * (Y - 1.0),
                r2 = 2.0 * rc
            ),
            ..base
        });
    }
    // Dashed and dotted underlines.
    for (name, dash) in [
        ("dashed-underline", "[3 2] 0 d"),
        ("dotted-underline", "[0.2 1.6] 0 d 1 J"),
    ] {
        out.push(shape(
            name,
            format!(
                "{}0 G 0.6 w {dash} 72 {y} m {rc} {y} l S\n",
                text(CAPS, 72.0, 740.0),
                y = Y + 0.3
            ),
            vec![[72.0, Y, rc, Y + 0.6]],
        ));
    }
    // The top of a box with a gap, half transparent, or in a color the text does not have.
    {
        let mut s = gap_frame(CAPS, 1.0, "solid");
        s.name = "half-transparent-top".into();
        s.content = s
            .content
            .replacen(&format!("72 {Y} "), &format!("q /Half gs 72 {Y} "), 1)
            .replacen(" re f\n", " re f Q\n", 1);
        out.push(s);
        let mut s = gap_frame(CAPS, 1.0, "solid");
        s.name = "red-top".into();
        s.content = s
            .content
            .replacen(&format!("72 {Y} "), &format!("1 0 0 rg 72 {Y} "), 1);
        out.push(s);
    }
    // Double underline; an underline with a table's rule 2.5 pt under it.
    out.push(ruled(
        "double",
        CAPS,
        &format!("0 g 72 {} {wc} 0.4 re f\n", Y - 1.4),
        vec![[72.0, Y - 1.4, rc, Y - 1.0]],
    ));
    out.push(ruled(
        "rule-below",
        CAPS,
        &format!("0 G 0.5 w 60 {y} m 400 {y} l S\n", y = Y - 2.5),
        vec![[60.0, Y - 2.75, 400.0, Y - 2.25]],
    ));
    // A line to sign on, to the page's edge; a contents line with a dotted leader.
    out.push(shape(
        "signature",
        format!(
            "{}0 G 0.5 w 72 {Y} m 560 {Y} l S\n",
            text("Signature", 72.0, 740.0)
        ),
        vec![[72.0, Y - 0.25, 560.0, Y + 0.25]],
    ));
    out.push(shape(
        "toc-leader",
        format!(
            "{}{}0 G 0.8 w [0.2 3] 0 d 1 J 140 741 m 500 741 l S\n",
            text("Chapter 1", 72.0, 740.0),
            text("5", 510.0, 740.0)
        ),
        vec![[140.0, 740.6, 500.0, 741.4]],
    ));
    // Separators: under a heading, wider than it; the page's width; above a footnote.
    out.push(shape(
        "separator-wide",
        format!(
            "{}0 G 0.5 w 72 737 m 400 737 l S\n",
            text("Gardening", 72.0, 740.0)
        ),
        vec![[72.0, 736.75, 400.0, 737.25]],
    ));
    out.push(shape(
        "separator-page",
        format!(
            "{}0 G 0.5 w 0 737 m 595 737 l S\n",
            text("Gardening", 72.0, 740.0)
        ),
        vec![[0.0, 736.75, 595.0, 737.25]],
    ));
    out.push(shape(
        "footnote-separator",
        format!(
            "{}0 G 0.4 w 72 100 m 216 100 l S\n{}",
            text(ROW, 72.0, 400.0),
            text("1 Seeds come from the shared store.", 72.0, 88.0)
        ),
        vec![[72.0, 99.8, 216.0, 100.2]],
    ));
    // A highlighter behind the line with its underline; a picture at the underline's end.
    out.push(shape(
        "marker",
        format!(
            ".95 .9 .2 rg 70 735 {} 18 re f 0 g\n{}0 g 72 {Y} {wc} 0.6 re f\n",
            wc + 4.0,
            text(CAPS, 72.0, 740.0)
        ),
        vec![[70.0, 735.0, 72.0, 753.0], [72.0, Y, rc, Y + 0.6]],
    ));
    out.push(ruled(
        "picture-at-end",
        CAPS,
        &format!(
            "q 8 0 0 8 {rc} {} cm BI /W 1 /H 1 /CS /G /BPC 8 ID 0 EI Q\n",
            Y - 4.0
        ),
        vec![[rc, Y - 4.0, rc + 8.0, Y + 4.0]],
    ));
    // Strike-through and overline.
    out.push(shape(
        "strike",
        format!("{}0 g 72 744 {wc} 0.6 re f\n", text(CAPS, 72.0, 740.0)),
        vec![[72.0, 744.0, rc, 744.6]],
    ));
    out.push(shape(
        "overline",
        format!("{}0 g 72 750 {wc} 0.6 re f\n", text(CAPS, 72.0, 740.0)),
        vec![[72.0, 750.0, rc, 750.6]],
    ));
    // A table's inner rules crossing the row's rule.
    out.push(ruled(
        "inner-table",
        CAPS,
        &format!(
            "0 G 0.5 w 72 720 m 72 760 l S {m} 720 m {m} 760 l S {rc} 720 m {rc} 760 l S\n",
            m = 72.0 + wc / 2.0
        ),
        vec![
            [71.75, 720.0, 72.25, 760.0],
            [rc - 0.25, 720.0, rc + 0.25, 760.0],
        ],
    ));
    // A box drawn as one rectangle, its top where an underline would be.
    out.push(shape(
        "box-one-path",
        format!(
            "{}0 G 0.4 w 72 720 {wc} {} re S\n",
            text(CAPS, 72.0, 740.0),
            Y + 0.3 - 720.0
        ),
        vec![[72.0, Y + 0.1, rc, Y + 0.5], [71.8, 720.0, 72.2, Y + 0.3]],
    ));
    out
}

// ---- Table rules drawn by PDF producers ----

/// A rule as pdfTeX draws one: a stroke 0.398 pt wide (or `weight`) in a space moved to its
/// left end, from 0 to `w`, centred at page height `y`.
fn tex_rule(x: f64, y: f64, w: f64, weight: f64) -> String {
    format!("q 1 0 0 1 {x} {y} cm [] 0 d 0 J {weight} w 0 0 m {w} 0 l S Q\n")
}

/// Text as pdfTeX sets lines, in one text object from (`x`, `y`): `lines` one under the other,
/// each `(text, down)` moved down from the one before.
fn tex_text(x: f64, y: f64, lines: &[(&str, f64)]) -> String {
    let mut out = format!("BT /F1 12 Tf 95 Tz 1 0 0 1 {x} {y} Tm");
    for (k, (s, down)) in lines.iter().enumerate() {
        if k > 0 {
            out.push_str(&format!(" 0 {} Td", -down));
        }
        out.push_str(&format!(" ({s}) Tj"));
    }
    out.push_str(" ET\n");
    out
}

/// The keep box of a stroked rule from `x` to `x + w`, `weight` thick, centred at `y`.
fn stroke_keep(x: f64, y: f64, w: f64, weight: f64) -> [f64; 4] {
    [x, y - weight / 2.0, x + w, y + weight / 2.0]
}

const TEX_ROWS: [&str; 4] = [
    ROW,
    "Healthy seedlings grow.",
    "Young plants need regular watering.",
    "Spring planting starts here.",
];

/// Two one-column tables with pdfLaTeX geometry
/// (`@{}l@{}`), each of two rows, the first row the wider; under it, `offset` pt below its
/// baseline, an `\hline` as long as it, and no other rule, side or box. 4.533 pt as LaTeX sets
/// it; 3.537, 3.039, 2.541 and 2.042 after `\\[-1pt]`, `[-1.5pt]`, `[-2pt]` and `[-2.5pt]`. The
/// drawing order matches pdfLaTeX, with rows 10.311 pt below a rule and 27.397 pt between tables.
pub(super) fn latex_tabular(offset: f64, name: &str) -> Shape {
    let b1 = 709.888;
    let r1 = b1 - offset;
    let b2 = r1 - 10.311;
    let b3 = b2 - 27.397;
    let r2 = b3 - offset;
    let b4 = r2 - 10.311;
    let (w1, w3) = (width(TEX_ROWS[0]), width(TEX_ROWS[2]));
    shape(
        name,
        format!(
            "{}{}{}{}{}",
            tex_text(72.0, b1, &[(TEX_ROWS[0], 0.0)]),
            tex_rule(72.0, r1, w1, 0.398),
            tex_text(72.0, b2, &[(TEX_ROWS[1], 0.0), (TEX_ROWS[2], 27.397)]),
            tex_rule(72.0, r2, w3, 0.398),
            tex_text(72.0, b4, &[(TEX_ROWS[3], 0.0)])
        ),
        vec![
            stroke_keep(72.0, r1, w1, 0.398),
            stroke_keep(72.0, r2, w3, 0.398),
        ],
    )
}

/// A table with booktabs spacing: `\toprule` (0.936 pt) 13.926 pt above the head's
/// baseline, `\midrule` (0.585) 6.686 below it, the rows 14.446 apart, `\bottomrule` 6.861
/// below the last, each from the first column's text's left to the last column's right. `cols`
/// are the columns' texts by row and whether each is set flush right; `gap` the space between
/// columns (12 pt, `\tabcolsep` twice).
fn booktabs(name: &str, rows: &[&[&str]], right: &[bool], gap: f64) -> Shape {
    let n = rows[0].len();
    let widths: Vec<f64> = (0..n)
        .map(|c| rows.iter().map(|r| width(r[c])).fold(0.0, f64::max))
        .collect();
    let mut starts = vec![72.0];
    for c in 1..n {
        starts.push(starts[c - 1] + widths[c - 1] + gap);
    }
    let end = starts[n - 1] + widths[n - 1];
    let base = 705.606;
    let mut content = tex_rule(72.0, base + 13.926, end - 72.0, 0.936);
    let mut keep = vec![stroke_keep(72.0, base + 13.926, end - 72.0, 0.936)];
    for (k, row) in rows.iter().enumerate() {
        let y = base
            - if k == 0 {
                0.0
            } else {
                20.436 + 14.446 * (k - 1) as f64
            };
        for (c, cell) in row.iter().enumerate() {
            let x = if right[c] {
                starts[c] + widths[c] - width(cell)
            } else {
                starts[c]
            };
            content.push_str(&tex_text(x, y, &[(cell, 0.0)]));
        }
        if k == 0 {
            content.push_str(&tex_rule(72.0, y - 6.686, end - 72.0, 0.585));
            keep.push(stroke_keep(72.0, y - 6.686, end - 72.0, 0.585));
        }
        if k == rows.len() - 1 {
            content.push_str(&tex_rule(72.0, y - 6.861, end - 72.0, 0.936));
            keep.push(stroke_keep(72.0, y - 6.861, end - 72.0, 0.936));
        }
    }
    shape(name, content, keep)
}

/// A three-line table (三線表) of two columns set 6 pt apart, closer than PDF.js's 0.6 em, so
/// that each row is one line from the first column's left to the second's right: the head's top
/// and bottom rules and the table's bottom rule, 0.5 pt strokes, each as long as every row, 3 pt
/// above the head and 3.5 below the head and the last row.
fn three_line_table() -> Shape {
    let rows = [
        ["Seeds", "Amount"],
        ["Tomato", "12 bags"],
        ["Basil", " 4 bags"],
    ];
    let w0 = 6.0 * 6.84;
    let x1 = 72.0 + w0 + 6.0;
    let end = x1 + 7.0 * 6.84;
    let mut content = String::new();
    let mut keep = Vec::new();
    let mut rule = |y: f64, content: &mut String| {
        content.push_str(&format!("0 G 0.5 w 72 {y} m {end} {y} l S\n"));
        keep.push([72.0, y - 0.25, end, y + 0.25]);
    };
    rule(752.0, &mut content);
    for (k, row) in rows.iter().enumerate() {
        let y = 740.0 - 16.0 * k as f64;
        content.push_str(&text(row[0], 72.0, y));
        content.push_str(&text(row[1], x1, y));
        if k == 0 || k == rows.len() - 1 {
            rule(y - 3.5, &mut content);
        }
    }
    shape("three-line-table", content, keep)
}

/// Rules that can resemble underlines: a box whose sides
/// stand 3.6 pt out from the top's ends (`offset-corners-3.6-0`), a fraction's bar, a line to
/// sign on, a table's head's rule above a row, rows each over a rule of their own width, rules of
/// a list of three rows.
pub(super) fn hand_made_shapes() -> Vec<Shape> {
    let w = width(ROW);
    let mut out = Vec::new();
    {
        let (l, r) = (72.0 - 3.6, 72.0 + w + 3.6);
        out.push(shape(
            "offset-corners-3.6-0",
            format!(
                "{}0 g 72 {Y} {w} 0.6 re f\n0 G 0.4 w {l} {Y} m {l} 685 l S {r} {Y} m {r} 685 l S \
                 {l} 685 m {r} 685 l S\n",
                plain(ROW, 72.0, 740.0)
            ),
            vec![
                [72.0, Y, 72.0 + w, Y + 0.6],
                [l - 0.2, 685.0, l + 0.2, Y],
                [r - 0.2, 685.0, r + 0.2, Y],
                [l, 684.8, r, 685.2],
            ],
        ));
    }
    let bar = |y: f64| format!("0 g 72 {y} {w} 0.6 re f\n");
    out.push(shape(
        "fraction-3.6-fill",
        format!(
            "{}{}{}",
            plain(ROW, 72.0, 740.0),
            bar(736.4),
            plain("Healthy roots make healthy soil.", 89.1, 721.0)
        ),
        vec![[72.0, 736.4, 72.0 + w, 737.0]],
    ));
    out.push(shape(
        "signature-3.6-stroke",
        format!(
            "{}{}0 G 0.6 w 72 736.7 m {} 736.7 l S\n",
            plain("Approved by the gardening group.", 72.0, 780.0),
            plain(ROW, 72.0, 740.0),
            72.0 + w
        ),
        vec![[72.0, 736.4, 72.0 + w, 737.0]],
    ));
    out.push(shape(
        "single-header-3.6-fill",
        format!(
            "{}{}{}",
            plain(ROW, 72.0, 740.0),
            bar(736.4),
            plain("Seasonal allocation for seedlings.", 72.0, 715.0)
        ),
        vec![[72.0, 736.4, 72.0 + w, 737.0]],
    ));
    {
        let second = "Healthy seedlings grow.";
        out.push(shape(
            "unequal-rows-1.6-fill",
            format!(
                "{}{}{}0 g 72 713.4 {} 0.6 re f\n",
                plain(ROW, 72.0, 740.0),
                bar(738.4),
                plain(second, 72.0, 715.0),
                width(second)
            ),
            vec![
                [72.0, 738.4, 72.0 + w, 739.0],
                [72.0, 713.4, 72.0 + width(second), 714.0],
            ],
        ));
    }
    {
        let rows = [
            ROW,
            "Spring planting starts here.",
            "Young trees need regular care.",
        ];
        let mut content = String::new();
        let mut keep = Vec::new();
        for (k, s) in rows.iter().enumerate() {
            let y = 740.0 - 20.0 * k as f64;
            content.push_str(&plain(s, 72.0, y));
            content.push_str(&format!("0 g 72 {} {} 0.6 re f\n", y - 1.6, width(s)));
            keep.push([72.0, y - 1.6, 72.0 + width(s), y - 1.0]);
        }
        out.push(shape("list-rules-0", content, keep));
    }
    out
}

/// Horizontal rules in synthetic tables and in layouts matching pdfTeX and booktabs.
/// None of the rules has a link annotation.
pub(super) fn table_rule_shapes() -> Vec<Shape> {
    let mut out = vec![latex_tabular(4.533, "latex-tabular")];
    for (adjust, offset) in [(1.0, 3.537), (1.5, 3.039), (2.0, 2.541), (2.5, 2.042)] {
        out.push(latex_tabular(
            offset,
            &format!("latex-tabular-minus{adjust}pt"),
        ));
    }
    out.push(booktabs(
        "booktabs-lrr",
        &[
            &["Bed", "Seedlings", "Water"],
            &["North", "12", "3"],
            &["South", "8", "2"],
        ],
        &[false, true, true],
        12.0,
    ));
    out.push(booktabs(
        "booktabs-l",
        &[&[TEX_ROWS[0]], &[TEX_ROWS[1]]],
        &[false],
        12.0,
    ));
    out.push(three_line_table());
    out
}

/// Link annotations over rules: over a table's cell, a box's top, a separator, two lines, more
/// than the rule, less than it, two links under one rule. Each keeps all its rules (as every shape
/// does: a link over a rule says nothing of what the rule is).
pub(super) fn link_abuse_shapes() -> Vec<Shape> {
    let w = width(ROW);
    let rc = 72.0 + width(CAPS);
    let mut out = Vec::new();
    // Hyperref-style links on the wider cells of one-column tables, `\hline`
    // under each (no other rule): as an underline, end to end, under its link. Each table's two
    // rows are read as one paragraph.
    let b1 = 709.888;
    for (name, offset) in [
        ("link-cell-hline", 4.533),
        ("link-cell-hline-minus1.5pt", 3.039),
    ] {
        let b3 = b1 - offset - 10.311 - 27.397;
        let s = with_link(
            latex_tabular(offset, ""),
            name,
            tex_link(72.0, 72.0 + w, b1, true),
        );
        out.push(with_link(
            s,
            name,
            tex_link(72.0, 72.0 + width(TEX_ROWS[2]), b3, true),
        ));
    }
    // The same table with its rows apart (each a paragraph of its own): the linked row's rule is
    // its whole line's underline, under its link.
    {
        let base = 709.888;
        let rule = base - 4.533;
        out.push(Shape {
            annots: vec![link_annot(tex_link(72.0, 72.0 + w, base, true))],
            ..shape(
                "link-cell-hline-row",
                format!(
                    "{}{}{}",
                    tex_text(72.0, base, &[(TEX_ROWS[0], 0.0)]),
                    tex_rule(72.0, rule, w, 0.398),
                    tex_text(72.0, rule - 22.0, &[(TEX_ROWS[1], 0.0)])
                ),
                vec![stroke_keep(72.0, rule, w, 0.398)],
            )
        });
    }
    // The same with the table's top rule too (`\hline` above the first row), as long.
    {
        let mut s = with_link(
            latex_tabular(4.533, ""),
            "link-cell-hline-top",
            tex_link(72.0, 72.0 + w, b1, true),
        );
        s.content = tex_rule(72.0, b1 + 11.5, w, 0.398) + &s.content;
        s.keep.push(stroke_keep(72.0, b1 + 11.5, w, 0.398));
        out.push(s);
    }
    // booktabs' one-column table, a link on its wider row.
    out.push(with_link(
        booktabs("", &[&[TEX_ROWS[0]], &[TEX_ROWS[1]]], &[false], 12.0),
        "link-cell-booktabs",
        tex_link(72.0, 72.0 + w, 705.606, true),
    ));
    // A table of horizontal rules drawn with its rows (two as long as the first row).
    out.push(with_link(
        more_shapes()
            .into_iter()
            .find(|s| s.name == "hrules-in-flow")
            .unwrap(),
        "link-hrules-in-flow",
        word_link(72.0, 72.0 + w, 740.0),
    ));
    // A cell of a ruled table (`|l|`), its text a link.
    out.push(with_link(
        more_shapes()
            .into_iter()
            .find(|s| s.name == "cell-touch")
            .unwrap(),
        "link-cell-touch",
        word_link(72.0, 72.0 + w, 740.0),
    ));
    // A linked label above a box whose sides stop 1 pt short of its top, or extend
    // 3.6 pt beyond its ends.
    out.push(with_link(
        gap_frame(CAPS, 1.0, "solid"),
        "link-box-top",
        word_link(72.0, rc, 740.0),
    ));
    out.push(with_link(
        gap_frame(ROW, 1.6, "solid"),
        "link-box-top-row",
        word_link(72.0, 72.0 + w, 740.0),
    ));
    out.push(with_link(
        hand_made_shapes().remove(0),
        "link-offset-corners-3.6-0",
        word_link(72.0, 72.0 + w, 740.0),
    ));
    // A separator under a heading that is a link: wider than it; as long, 8 pt below.
    out.push(with_link(
        more_shapes()
            .into_iter()
            .find(|s| s.name == "separator-wide")
            .unwrap(),
        "link-separator-wide",
        word_link(72.0, 72.0 + width("Gardening"), 740.0),
    ));
    out.push(shape(
        "link-separator-below",
        format!(
            "{}0 G 0.5 w 72 732 m {} 732 l S\n",
            text("Gardening", 72.0, 740.0),
            72.0 + width("Gardening")
        ),
        vec![[72.0, 731.75, 72.0 + width("Gardening"), 732.25]],
    ));
    if let Some(s) = out.last_mut() {
        s.annots.push(link_annot(word_link(
            72.0,
            72.0 + width("Gardening"),
            740.0,
        )));
    }
    // One link annotation over both lines of a paragraph, each line underlined end to end.
    {
        let second = "Water the seedlings every day.";
        out.push(Shape {
            annots: vec![link_annot([69.75, 721.6, 72.0 + w + 2.2, 750.1])],
            ..shape(
                "link-over-two-lines",
                format!(
                    "{}0 g 72 {Y} {w} 0.6 re f*\n{}0 g 72 724.4 {} 0.6 re f*\n",
                    plain(ROW, 72.0, 740.0),
                    plain(second, 72.0, 726.0),
                    width(second)
                ),
                vec![
                    [72.0, Y, 72.0 + w, Y + 0.6],
                    [72.0, 724.4, 72.0 + width(second), 725.0],
                ],
            )
        });
    }
    // A link 30 pt wider than its underline on either side (over the margins of a cell, say).
    out.push(Shape {
        annots: vec![link_annot([42.0, 737.6, 72.0 + w + 30.0, 750.1])],
        ..shape(
            "link-wider",
            format!("{}0 g 72 {Y} {w} 0.6 re f*\n", plain(ROW, 72.0, 740.0)),
            vec![[72.0, Y, 72.0 + w, Y + 0.6]],
        )
    });
    // A link on two words of a line underlined end to end.
    {
        let c = 7.2 * 0.95;
        out.push(Shape {
            annots: vec![link_annot(word_link(
                72.0 + 10.0 * c,
                72.0 + 15.0 * c,
                740.0,
            ))],
            ..shape(
                "link-narrower",
                format!("{}0 g 72 {Y} {w} 0.6 re f*\n", plain(ROW, 72.0, 740.0)),
                vec![[72.0, Y, 72.0 + w, Y + 0.6]],
            )
        });
        // Two links side by side under one underline.
        out.push(Shape {
            annots: vec![
                link_annot(word_link(72.0, 72.0 + 15.0 * c, 740.0)),
                link_annot(word_link(72.0 + 16.0 * c, 72.0 + w, 740.0)),
            ],
            ..shape(
                "link-two-under-one",
                format!("{}0 g 72 {Y} {w} 0.6 re f*\n", plain(ROW, 72.0, 740.0)),
                vec![[72.0, Y, 72.0 + w, Y + 0.6]],
            )
        });
    }
    out
}

/// A pdfLaTeX-style one-column table. The wider cell is linked and has a single rule
/// `offset` pt below its baseline. The next row is `down` pt below the rule.
/// The rows are separate text objects, with a 0.398 pt stroke between them.
/// The link rectangle spans the cell as in hyperref.
pub(super) fn linked_table(name: &str, offset: f64, down: f64) -> Shape {
    let base = 709.888;
    let rule = base - offset;
    let w = width(ROW);
    Shape {
        annots: vec![link_annot(tex_link(72.0, 72.0 + w, base, true))],
        ..shape(
            name,
            format!(
                "{}{}{}",
                tex_text(72.0, base, &[(ROW, 0.0)]),
                tex_rule(72.0, rule, w, 0.398),
                tex_text(72.0, rule - down, &[(TEX_ROWS[1], 0.0)])
            ),
            vec![stroke_keep(72.0, rule, w, 0.398)],
        )
    }
}

/// Eight linked cells whose table rules must stay. Vary how far the rule is raised above its
/// normal position (1 to 2.5 pt) or vary the gap to the next row (8 to 20 pt).
pub(super) fn linked_table_shapes() -> Vec<Shape> {
    let mut out = Vec::new();
    for (raise, rule) in [
        ("1", 706.351),
        ("1.5", 706.849),
        ("2", 707.347),
        ("2.5", 707.846),
    ] {
        // The next row 24.259 pt below the rule in each.
        out.push(linked_table(
            &format!("linked-table-close-{raise}"),
            709.888 - rule,
            24.259,
        ));
    }
    for (skip, next) in [(8, 687.074), (12, 683.088), (14, 681.096), (20, 675.118)] {
        out.push(linked_table(
            &format!("linked-table-{skip}-tight"),
            709.888 - 705.355,
            705.355 - next,
        ));
    }
    out
}

// ---- Underlines as producers draw them ----

/// Underlines as PDF producers draw them, with no link annotation: the row (100%
/// width) underlined across, its descenders touching the rule, or some of its words. No link says
/// that the line is the text's: each is kept as it is drawn (`keep`), its line too.
pub(super) fn producer_shapes() -> Vec<Shape> {
    let row = ROW;
    let w = width(row);
    let r = 72.0 + w;
    let c = 7.2 * 0.95;
    let fill = |x: f64, w: f64| [x, Y, x + w, Y + 0.6];
    let mut out = Vec::new();
    // Word: each run of text in its BT/ET, then its underline as a filled rectangle (`re f*`).
    out.push(shape(
        "word",
        format!("{}0 g 72 {Y} {w} 0.6 re f*\n", plain(row, 72.0, 740.0)),
        vec![fill(72.0, w)],
    ));
    // Word, a paragraph that begins with underlined words (a link's, without its annotation).
    out.push(shape(
        "word-first-words",
        format!(
            "BT 0 0 0.8 rg /F1 12 Tf 95 Tz 1 0 0 1 72 740 Tm (gardening group) Tj ET\n\
             0 0 0.8 rg 72 {Y} {b} 0.6 re f*\n0 g {}",
            plain(
                " meets each week in the shared beds.",
                72.0 + 15.0 * c,
                740.0
            ),
            b = 15.0 * c
        ),
        vec![fill(72.0, 15.0 * c)],
    ));
    // Word, an underline of two runs: one rectangle after each.
    out.push(shape(
        "word-two-runs",
        format!(
            "{}0 g 72 {Y} {a} 0.6 re f*\n{}0 g {m} {Y} {b} 0.6 re f*\n",
            plain("Gardening, ", 72.0, 740.0),
            plain("group keeps the beds green.", 72.0 + 11.0 * c, 740.0),
            a = 11.0 * c,
            m = 72.0 + 11.0 * c,
            b = 27.0 * c
        ),
        vec![fill(72.0, 38.0 * c)],
    ));
    // LibreOffice: the page clipped, the underline a stroke after its text.
    out.push(shape(
        "libreoffice",
        format!(
            "q 0 0 595 842 re W n\n{}0 G 0.6 w 72 {y} m {r} {y} l S\nQ\n",
            plain(row, 72.0, 740.0),
            y = Y + 0.3
        ),
        vec![fill(72.0, w)],
    ));
    // Chrome (Skia): pixels in a flipped space, each run clipped, the underline before its text.
    {
        let px = |y: f64| (842.0 - y) / 0.75;
        out.push(shape(
            "chrome",
            format!(
                "q 0.75 0 0 -0.75 0 842 cm q 90 {} {} 40 re W n 0 g 96 {} {} 0.8 re f \
                 BT /F1 16 Tf 95 Tz 1 0 0 -1 96 {} Tm ({row}) Tj ET Q Q\n",
                px(760.0),
                w / 0.75 + 20.0,
                px(Y + 0.6),
                w / 0.75,
                px(740.0)
            ),
            vec![fill(72.0, w)],
        ));
    }
    // pdfTeX's `\underline`: a stroke in a translated space, after the text.
    out.push(shape(
        "pdftex",
        format!(
            "{}q 1 0 0 1 72 {y} cm [] 0 d 0 J 0.398 w 0 0 m {w} 0 l S Q\n",
            plain(row, 72.0, 740.0),
            y = Y + 0.3
        ),
        vec![stroke_keep(72.0, Y + 0.3, w, 0.398)],
    ));
    // ulem's `\uline`: a piece under each word and each space, between the words.
    out.push(shape(
        "ulem",
        ulem(row, 72.0, 740.0, Y + 0.3),
        vec![stroke_keep(72.0, Y + 0.3, w, 0.398)],
    ));
    // PowerPoint: the text box clipped, the underline a filled rectangle after its text.
    out.push(shape(
        "powerpoint",
        format!(
            "q 60 700 {} 70 re W n {}0 g 72 {Y} {w} 0.6 re f* Q\n",
            w + 30.0,
            plain(row, 72.0, 740.0)
        ),
        vec![fill(72.0, w)],
    ));
    // Two paragraphs side by side, each underlined at the same height.
    {
        let (a, b) = ("Gardening group", "Separate right column.");
        out.push(shape(
            "two-columns",
            format!(
                "{}0 g 72 {Y} {} 0.6 re f*\n{}0 g 300 {Y} {} 0.6 re f*\n",
                plain(a, 72.0, 740.0),
                a.len() as f64 * c,
                plain(b, 300.0, 740.0),
                b.len() as f64 * c
            ),
            vec![
                fill(72.0, a.len() as f64 * c),
                fill(300.0, b.len() as f64 * c),
            ],
        ));
    }
    // An underlined heading above a paragraph that is not underlined.
    out.push(shape(
        "heading-and-body",
        format!(
            "{}0 g 72 {Y} {} 0.6 re f*\n{}{}",
            plain("Gardening group", 72.0, 740.0),
            15.0 * c,
            plain("Our group meets each week in the shared beds.", 72.0, 712.0),
            plain("Bring your own gloves and a water bottle.", 72.0, 698.0)
        ),
        vec![fill(72.0, 15.0 * c)],
    ));
    // Chrome's underline of a link, under its annotation, but clipped around the descenders that
    // cross it (`text-decoration-skip-ink`): a clip of holes, which [`rule_flaw`] takes as a
    // rule not drawn whole.
    {
        let (x0, x1) = (72.0 + 4.0 * c, 72.0 + 19.0 * c);
        let hole = |x: f64| format!("{} 737.9 3 1.5 re ", x);
        out.push(Shape {
            annots: vec![link_annot([x0 - 0.09, 736.65, x1 + 0.25, 750.15])],
            ..shape(
                "chrome-link-skip-ink",
                format!(
                    "{}BT 0 0 0.93 rg /F1 12 Tf 95 Tz 1 0 0 1 {x0} 740 Tm (gardening group) Tj ET\n\
                     q 0 0 595 842 re {}{}W* n 0 0 0.93 rg {x0} 738.15 {} 0.75 re f Q\n0 g {}",
                    plain("Our ", 72.0, 740.0),
                    hole(x0 + 0.5 * c),
                    hole(x0 + 10.5 * c),
                    15.0 * c,
                    plain(" meets each week.", x1, 740.0)
                ),
                vec![[x0, 738.15, x0 + 0.4 * c, 738.9]],
            )
        });
    }
    out
}

/// ulem's `\uline` of `row` from (`x`, `base`): a piece under each word and each space, between
/// the words, centred at `y`.
fn ulem(row: &str, x: f64, base: f64, y: f64) -> String {
    let c = 7.2 * 0.95;
    let mut content = String::new();
    let mut x = x;
    for (k, word) in row.split(' ').enumerate() {
        if k > 0 {
            content.push_str(&format!(
                "q 1 0 0 1 {} {y} cm 0.398 w 0 0 m {} 0 l S Q\n",
                x - 0.2,
                c + 0.4
            ));
            x += c;
        }
        content.push_str(&plain(word, x, base));
        let wl = word.len() as f64 * c;
        content.push_str(&format!(
            "q 1 0 0 1 {} {y} cm 0.398 w 0 0 m {} 0 l S Q\n",
            x - 0.2,
            wl + 0.4
        ));
        x += wl;
    }
    content
}

/// Producer-specific underline and annotation geometry for links over part of a line
/// (`partial`) or over the whole line. Each rule stays as drawn.
pub(super) fn linked_producer_shapes() -> Vec<(Shape, bool)> {
    let c = 7.2 * 0.95;
    let w = width(ROW);
    let r = 72.0 + w;
    let (x0, x1) = (72.0 + 4.0 * c, 72.0 + 19.0 * c);
    let blue = "0 0 0.8 rg";
    // "Our " black, "gardening group" in the link's color, " meets each week." black; the
    // link's underline (`rule`, drawn in its color) right after the link's words.
    let sentence = |color: &str, rule: &str| {
        format!(
            "{}BT {color} /F1 12 Tf 95 Tz 1 0 0 1 {x0} 740 Tm (gardening group) Tj ET\n{rule}0 g {}",
            plain("Our ", 72.0, 740.0),
            plain(" meets each week.", x1, 740.0)
        )
    };
    let mut out = Vec::new();
    let linked = |name: &str, content: String, annots: Vec<[f64; 4]>| Shape {
        annots: annots.into_iter().map(link_annot).collect(),
        ..shape(name, content, vec![])
    };
    // Word: the link's words blue, their underline a blue rectangle after them; the annotation
    // covers the text box or the 18.5 pt line pitch, extending past the linked words.
    out.push((
        linked(
            "word-link",
            sentence(blue, &format!("{blue} {x0} {Y} {} 0.6 re f*\n", 15.0 * c)),
            vec![word_link(x0, x1, 740.0)],
        ),
        true,
    ));
    out.push((
        linked(
            "word-link-pitch",
            sentence(blue, &format!("{blue} {x0} {Y} {} 0.6 re f*\n", 15.0 * c)),
            vec![[x0 - 2.25, 731.6, x1 + 2.2, 750.1]],
        ),
        true,
    ));
    // Word, a paragraph that begins with a link's words.
    out.push((
        linked(
            "word-link-first",
            format!(
                "BT {blue} /F1 12 Tf 95 Tz 1 0 0 1 72 740 Tm (gardening group) Tj ET\n\
                 {blue} 72 {Y} {b} 0.6 re f*\n0 g {}",
                plain(
                    " meets each week in the shared beds.",
                    72.0 + 15.0 * c,
                    740.0
                ),
                b = 15.0 * c
            ),
            vec![word_link(72.0, 72.0 + 15.0 * c, 740.0)],
        ),
        true,
    ));
    // Word, a line that is a link end to end: underlined again in the translation.
    out.push((
        linked(
            "word-link-line",
            format!(
                "BT {blue} /F1 12 Tf 95 Tz 1 0 0 1 72 740 Tm ({ROW}) Tj ET\n{blue} 72 {Y} {w} 0.6 re f*\n"
            ),
            vec![word_link(72.0, r, 740.0)],
        ),
        false,
    ));
    // Word, two links of a line, each under its own annotation ("gardening", "week"; most of
    // the line black).
    out.push((
        linked(
            "word-two-links",
            format!(
                "{}BT {blue} /F1 12 Tf 95 Tz 1 0 0 1 {x0} 740 Tm (gardening) Tj ET\n\
                 {blue} {x0} {Y} {a} 0.6 re f*\n0 g {}BT {blue} /F1 12 Tf 95 Tz 1 0 0 1 {m} 740 Tm \
                 (week) Tj ET\n{blue} {m} {Y} {b} 0.6 re f*\n0 g {}",
                plain("Our ", 72.0, 740.0),
                plain(" group meets each ", x0 + 9.0 * c, 740.0),
                plain(".", 72.0 + 35.0 * c, 740.0),
                a = 9.0 * c,
                m = 72.0 + 31.0 * c,
                b = 4.0 * c
            ),
            vec![
                word_link(x0, x0 + 9.0 * c, 740.0),
                word_link(72.0 + 31.0 * c, 72.0 + 35.0 * c, 740.0),
            ],
        ),
        true,
    ));
    // LibreOffice: the link's words in its color, its underline a stroke in a moved space right
    // after them; the annotation from 0.11 pt left of the words, 2.75 below the baseline to 9.25
    // above (the producer's file).
    out.push((
        linked(
            "libreoffice-link",
            sentence(
                "0 0 0.502 rg",
                &format!(
                    "q 1 0 0 1 {x0} 740 cm 0.6 w 0 0 0.502 RG 0 -1.3 m {} -1.3 l S Q\n",
                    15.0 * c
                ),
            ),
            vec![[x0 - 0.11, 737.25, x1 - 0.04, 749.25]],
        ),
        true,
    ));
    // Chrome: pixels in a flipped space, each element clipped to its box, the link's underline
    // (0.75 pt, 1.1 below the baseline) right after its words; the annotation the line's box
    // (13.5 pt), from 0.09 pt left of the words (the producer's file).
    {
        let px = |y: f64| (842.0 - y) / 0.75;
        let (pa, pb) = (x0 / 0.75, x1 / 0.75);
        out.push((
            linked(
                "chrome-link",
                format!(
                    "{}q 0.75 0 0 -0.75 0 842 cm q {} {} {} {} re W n 0 0 0.93 rg \
                     BT /F1 16 Tf 95 Tz 1 0 0 -1 {pa} {} Tm (gardening group) Tj ET \
                     {pa} {} {} 1 re f Q Q\n0 g {}",
                    plain("Our ", 72.0, 740.0),
                    pa - 1.0,
                    px(752.0),
                    pb - pa + 2.0,
                    16.0 / 0.75 + 4.0,
                    px(740.0),
                    px(738.9),
                    pb - pa,
                    plain(" meets each week.", x1, 740.0)
                ),
                vec![[x0 - 0.09, 736.65, x1 + 0.25, 750.15]],
            ),
            true,
        ));
    }
    // pdfTeX, `\href{…}{\underline{…}}`: the rule, 3.72 pt below the baseline, a stroke after
    // the link's words; hyperref's annotation 1 pt around their box.
    out.push((
        linked(
            "pdftex-hyperref",
            sentence("0 g", &tex_rule(x0, 736.28, 15.0 * c, 0.398)),
            vec![tex_link(x0, x1, 740.0, false)],
        ),
        true,
    ));
    // pdfTeX, `\href{…}{\uline{…}}`: ulem's pieces, 3.19 pt below the baseline, between the
    // words; the annotation 0.756 pt around them, from 4.383 below the baseline to 9.299 above.
    out.push((
        linked(
            "pdftex-hyperref-uline",
            format!(
                "{}{}{}",
                plain("Our ", 72.0, 740.0),
                ulem("gardening group", x0, 740.0, 736.81),
                plain(" meets each week.", x1, 740.0)
            ),
            vec![[x0 - 0.756, 735.617, x1 + 0.756, 749.299]],
        ),
        true,
    ));
    // PowerPoint: the text box clipped, the link's underline (0.48 pt) a filled rectangle after
    // its words; the annotation tight on the words, the rule 0.44 pt below its foot, or 1.13
    // (O-techreg).
    for (name, below) in [("powerpoint-link", 0.44), ("powerpoint-link-low", 1.13)] {
        let mid = Y + 0.24;
        out.push((
            linked(
                name,
                format!(
                    "q 60 700 {} 70 re W n {}Q\n",
                    w + 30.0,
                    sentence(blue, &format!("{blue} {x0} {Y} {} 0.48 re f*\n", 15.0 * c))
                ),
                vec![[x0 + 0.06, mid + below, x1 - 0.3, mid + below + 11.4]],
            ),
            true,
        ));
    }
    out
}

/// The shapes' outcomes, one line each, for the record (no text of a document: these are made
/// here).
fn report(label: &str, rows: &[(String, Run)]) {
    for (name, run) in rows {
        eprintln!(
            "{label}\t{name}\t{}\tlost={}\tlost-before-checks={}\tplaced={}\tsaved={:?}\t\
             before-checks={:?}",
            run.kind.map_or("error", PageKind::name),
            run.lost,
            run.lost_placement,
            !run.placed.is_empty(),
            run.saved,
            run.placement
        );
    }
}

/// Shapes whose rules must stay: frames, table rules, rules near links and unlinked underlines.
pub(super) fn negative_shapes() -> Vec<Shape> {
    let mut shapes = cell_and_frame_shapes();
    shapes.extend(more_shapes());
    shapes.extend(table_rule_shapes());
    shapes.extend(hand_made_shapes());
    shapes.extend(link_abuse_shapes());
    shapes.extend(producer_shapes());
    shapes.extend(linked_table_shapes());
    shapes
}

#[test]
#[ignore = "requires local PDFium and IPA font"]
fn a_save_draws_every_path_as_its_source_did() {
    // Every shape here, the negative and the positive (underlines, rules of tables, boxes,
    // separators, links over them): each page of the save draws every path of its source page,
    // point for point, in the same paint, clip and order, and no other; its drawing, the text
    // taken off, is the source's to the pixel. So is the placement's, before the checks of the
    // save: no line is taken off, moved, made longer or shorter, or drawn anew, on any page.
    let mut shapes = negative_shapes();
    shapes.extend(linked_producer_shapes().into_iter().map(|(s, _)| s));
    assert_eq!(shapes.len(), 147);
    let rows: Vec<_> = shapes
        .iter()
        .map(|s| (s.name.clone(), translate(s)))
        .collect();
    report("paths", &rows);
    let changed: Vec<_> = rows
        .iter()
        .filter(|(_, run)| run.saved.is_some_and(|c| c != (0, 0)) || run.placement != (0, 0))
        .map(|(n, run)| {
            format!(
                "{n} (saved {:?}, before checks {:?})",
                run.saved, run.placement
            )
        })
        .collect();
    assert!(changed.is_empty(), "drawn otherwise: {changed:?}");
    // The save fails on the turned pages only (`/Rotate`, the bilingual writer's failure
    // before this work); their placement is held to the same.
    let failed: Vec<_> = rows
        .iter()
        .filter(|(_, run)| run.saved.is_none())
        .map(|(n, _)| n.as_str())
        .collect();
    assert!(failed.iter().all(|n| n.contains("-rotated-")), "{failed:?}");
}

#[test]
#[ignore = "requires local PDFium and IPA font"]
fn no_box_or_rule_loses_ink_for_an_underline() {
    let shapes = negative_shapes();
    assert_eq!(shapes.len(), 136);
    let rows: Vec<_> = shapes
        .iter()
        .map(|s| (s.name.clone(), translate(s)))
        .collect();
    report("negative", &rows);
    // What is saved keeps all of it; and where the save failed, so does what the placement
    // drew. No shape is let pass.
    let broken: Vec<_> = rows
        .iter()
        .filter(|(_, run)| run.lost > 0 || (run.kind.is_none() && run.lost_placement > 0))
        .map(|(n, run)| format!("{n} ({}, {})", run.lost, run.lost_placement))
        .collect();
    assert!(broken.is_empty(), "ink lost: {broken:?}");
}

#[test]
#[ignore = "requires local PDFium and IPA font"]
fn an_underline_stays_as_drawn_under_a_link_or_not() {
    // Underlines as Word, LibreOffice, Chrome, pdfTeX and PowerPoint draw them: with no link
    // annotation over them, or under their link's (`true`: on some words of the line). Each is
    // left as it is drawn, every rule of the page there as before. A line underlined end to end
    // is set over its rule, clear of it; a line underlined in some words stays as it is (its
    // translation goes to the page added after it).
    let mut shapes: Vec<(Shape, bool)> = producer_shapes()
        .into_iter()
        .map(|s| {
            let partial = ["word-first-words", "two-columns", "chrome-link-skip-ink"]
                .contains(&s.name.as_str());
            (s, partial)
        })
        .collect();
    shapes.extend(linked_producer_shapes());
    let rows: Vec<_> = shapes
        .iter()
        .map(|(s, _)| (s.name.clone(), translate(s)))
        .collect();
    report("underlines", &rows);
    for ((shape, partial), (name, run)) in shapes.iter().zip(&rows) {
        assert_eq!(&shape.name, name);
        assert_eq!(run.lost, 0, "{name}");
        assert_eq!(run.saved, Some((0, 0)), "{name}");
        assert_eq!(run.rules, rules_on(&pdf(shape), 0), "{name}");
        // The underlined line: the first paragraph. The heading of `heading-and-body` is not
        // set either: its translation is longer than it, and in one line over its rule (which
        // it may not be widened past) it would be too small; it stays as it is.
        let underlined = run.placed.iter().any(|id| id == "p1-0");
        assert_eq!(underlined, !partial && name != "heading-and-body", "{name}");
    }
}

/// How many thin rules the page of `bytes` draws (an underline kept, a table's rule).
fn rules_on(bytes: &[u8], page: usize) -> usize {
    let (_guard, pdfium) = pdfium().unwrap();
    let doc = Doc::load(pdfium.bindings(), bytes).unwrap();
    let p = doc.page(page).unwrap();
    scan(doc.lib, &p).decorations.len()
}
