//! Conservative content-stream eraser. Unedited operators retain their exact decoded bytes.
//! PDFium supplies target origins; PDF advances are computed independently from font widths.
//! A paragraph is changed only when every selected character has one unambiguous match.
use std::collections::{BTreeMap, HashMap, HashSet};
use std::ops::Range;

use super::inplace::{self, EraseChar};
use lopdf::{Dictionary, Document, IncrementalDocument, Object, StringFormat, content::Content};

#[derive(Debug, thiserror::Error)]
pub enum Error {
    #[error("cannot edit PDF: {0}")]
    Pdf(#[from] lopdf::Error),
    #[error("cannot verify PDF erasure: {0}")]
    Verification(#[from] super::Error),
    #[error("protected PDFs are not supported by the eraser")]
    Protected,
    #[error("xref/object mismatch: {0}")]
    IncompleteObjects(String),
    #[error("cannot save PDF: {0}")]
    Io(#[from] std::io::Error),
}

impl From<super::parser::Error> for Error {
    fn from(error: super::parser::Error) -> Self {
        match error {
            super::parser::Error::Protected => Self::Protected,
            super::parser::Error::Pdf(e) => Self::Pdf(e),
            super::parser::Error::Incomplete(reason) => Self::IncompleteObjects(reason.into()),
        }
    }
}

#[derive(Debug, Default)]
pub struct Report {
    pub erased: Vec<String>,
    pub retained: BTreeMap<String, String>,
    pub targeted_characters: usize,
    pub matched_characters: usize,
    /// Page and font/operator diagnostics, never document text.
    pub diagnostics: Vec<String>,
}

type Matrix = [f64; 6];
const ID: Matrix = [1., 0., 0., 1., 0., 0.];
fn apply(m: Matrix, x: f64, y: f64) -> (f64, f64) {
    (m[0] * x + m[2] * y + m[4], m[1] * x + m[3] * y + m[5])
}
fn then(a: Matrix, b: Matrix) -> Matrix {
    let p = apply(b, a[4], a[5]);
    [
        b[0] * a[0] + b[2] * a[1],
        b[1] * a[0] + b[3] * a[1],
        b[0] * a[2] + b[2] * a[3],
        b[1] * a[2] + b[3] * a[3],
        p.0,
        p.1,
    ]
}
fn number(o: &Object) -> Option<f64> {
    o.as_float().ok().map(f64::from)
}
fn resolved<'a>(doc: &'a Document, o: &'a Object) -> Option<&'a Object> {
    doc.dereference(o).ok().map(|(_, o)| o)
}
fn field<'a>(doc: &'a Document, d: &'a Dictionary, k: &[u8]) -> Option<&'a Object> {
    resolved(doc, d.get(k).ok()?)
}

struct Font {
    bytes: usize,
    widths: HashMap<u32, f64>,
    default: Option<f64>,
    unicode: HashMap<u32, Vec<u32>>,
}
impl Font {
    fn read(doc: &Document, d: &Dictionary) -> Option<Self> {
        let mut font = Self::metrics(doc, d)?;
        // Force the ToUnicode branch: a simple font's Encoding can otherwise hide its CMap.
        let limit = super::parser::STREAM_LIMIT;
        let mut forced = d.clone();
        forced.set("Encoding", "Identity-H");
        let mut declared = d.clone();
        if let Some(base) = field(doc, d, b"Encoding")
            .and_then(|o| o.as_dict().ok())
            .and_then(|d| d.get(b"BaseEncoding").ok())
        {
            declared.set("Encoding", base.clone());
        }
        let mut cmap = d.has(b"ToUnicode");
        let mut encoding = None;
        if cmap {
            encoding = forced.get_font_encoding_with_limit(doc, limit).ok();
            // lopdf silently decodes with StandardEncoding when it cannot parse a CMap.
            // A simple font then uses its own declared encoding, exactly as a font
            // without ToUnicode does. Erasure still requires PDFium to read the same
            // character at the same origin; the post-erasure check is unchanged.
            if font.bytes == 1 && !matches!(encoding, Some(lopdf::Encoding::UnicodeMapEncoding(_)))
            {
                cmap = false;
            }
        }
        if !cmap {
            encoding = declared.get_font_encoding_with_limit(doc, limit).ok();
        }
        if let Some(encoding) = encoding {
            for code in 0..if font.bytes == 1 { 256u32 } else { 65536u32 } {
                let bytes = code.to_be_bytes();
                if let Ok(s) = encoding.bytes_to_string(&bytes[4 - font.bytes..]) {
                    let expanded = s
                        .replace('ﬀ', "ff")
                        .replace('ﬁ', "fi")
                        .replace('ﬂ', "fl")
                        .replace('ﬃ', "ffi")
                        .replace('ﬄ', "ffl");
                    if !expanded.is_empty() && !expanded.contains('�') {
                        font.unicode
                            .insert(code, expanded.chars().map(u32::from).collect());
                    }
                }
            }
        }
        if let Some(differences) = field(doc, d, b"Encoding")
            .and_then(|o| o.as_dict().ok())
            .and_then(|d| field(doc, d, b"Differences"))
            .and_then(|o| o.as_array().ok())
        {
            let mut code = 0;
            for entry in differences {
                if let Ok(n) = entry.as_i64() {
                    code = n as u32;
                } else if let Ok(name) = entry.as_name() {
                    if !cmap {
                        // An explicit Differences entry replaces the base encoding, even when
                        // its glyph name is unsupported. Never guess from the character code.
                        font.unicode.remove(&code);
                        if let Some(chars) = glyph_unicode(name) {
                            font.unicode.insert(code, chars);
                        }
                    }
                    code += 1;
                }
            }
        }
        Some(font)
    }

    fn metrics(doc: &Document, d: &Dictionary) -> Option<Self> {
        let subtype = d.get(b"Subtype").ok()?.as_name().ok()?;
        let mut widths = HashMap::new();
        if subtype == b"Type0" {
            if d.get(b"Encoding").ok()?.as_name().ok()? != b"Identity-H" {
                return None;
            }
            let descendant = field(doc, d, b"DescendantFonts")?
                .as_array()
                .ok()?
                .first()?;
            let cid = resolved(doc, descendant)?.as_dict().ok()?;
            let default = field(doc, cid, b"DW").and_then(number).unwrap_or(1000.);
            if let Some(w) = field(doc, cid, b"W").and_then(|o| o.as_array().ok()) {
                let mut i = 0;
                while i < w.len() {
                    let first = w.get(i)?.as_i64().ok()? as u32;
                    let next = resolved(doc, w.get(i + 1)?)?;
                    if let Ok(a) = next.as_array() {
                        for (j, v) in a.iter().enumerate() {
                            widths.insert(first + j as u32, number(v)?);
                        }
                        i += 2;
                    } else {
                        let last = next.as_i64().ok()? as u32;
                        if last < first || last - first > 65536 {
                            return None;
                        }
                        let width = number(w.get(i + 2)?)?;
                        for c in first..=last {
                            widths.insert(c, width);
                        }
                        i += 3;
                    }
                }
            }
            Some(Self {
                bytes: 2,
                widths,
                default: Some(default),
                unicode: HashMap::new(),
            })
        } else if subtype == b"TrueType" || subtype == b"Type1" {
            let first = field(doc, d, b"FirstChar")?.as_i64().ok()? as u32;
            for (i, w) in field(doc, d, b"Widths")?
                .as_array()
                .ok()?
                .iter()
                .enumerate()
            {
                widths.insert(first + i as u32, number(w)?);
            }
            let default = field(doc, d, b"FontDescriptor")
                .and_then(|o| o.as_dict().ok())
                .and_then(|d| field(doc, d, b"MissingWidth"))
                .and_then(number);
            Some(Self {
                bytes: 1,
                widths,
                default,
                unicode: HashMap::new(),
            })
        } else {
            None
        }
    }
}

// The Adobe glyph names needed for simple Latin fonts. Other names remain unsupported;
// ToUnicode takes precedence when present. Ligatures match PDFium's expanded characters.
fn glyph_unicode(name: &[u8]) -> Option<Vec<u32>> {
    if name.len() == 1 && name[0].is_ascii_alphabetic() {
        return Some(vec![u32::from(name[0])]);
    }
    let value = match name {
        b"space" => " ",
        b"exclam" => "!",
        b"quotedbl" => "\"",
        b"numbersign" => "#",
        b"dollar" => "$",
        b"percent" => "%",
        b"ampersand" => "&",
        b"quotesingle" => "'",
        b"parenleft" => "(",
        b"parenright" => ")",
        b"asterisk" => "*",
        b"plus" => "+",
        b"comma" => ",",
        b"hyphen" => "-",
        b"period" => ".",
        b"slash" => "/",
        b"zero" => "0",
        b"one" => "1",
        b"two" => "2",
        b"three" => "3",
        b"four" => "4",
        b"five" => "5",
        b"six" => "6",
        b"seven" => "7",
        b"eight" => "8",
        b"nine" => "9",
        b"colon" => ":",
        b"semicolon" => ";",
        b"less" => "<",
        b"equal" => "=",
        b"greater" => ">",
        b"question" => "?",
        b"at" => "@",
        b"bracketleft" => "[",
        b"backslash" => "\\",
        b"bracketright" => "]",
        b"asciicircum" => "^",
        b"underscore" => "_",
        b"grave" => "`",
        b"braceleft" => "{",
        b"bar" => "|",
        b"braceright" => "}",
        b"asciitilde" => "~",
        b"quoteleft" => "‘",
        b"quoteright" => "’",
        b"quotedblleft" => "“",
        b"quotedblright" => "”",
        b"bullet" => "•",
        b"endash" => "–",
        b"emdash" => "—",
        b"dotlessi" => "ı",
        b"dieresis" => "¨",
        b"fi" => "fi",
        b"fl" => "fl",
        b"ff" => "ff",
        b"ffi" => "ffi",
        b"ffl" => "ffl",
        _ => return None,
    };
    Some(value.chars().map(u32::from).collect())
}

/// Lex only enough to delimit operations. Strings, comments, arrays and dictionaries stay
/// opaque here; lopdf decodes their values. Inline images conservatively reject this page.
type OperationSpan = (Range<usize>, lopdf::content::Operation, bool);
fn spans(data: &[u8], boundaries: &[usize]) -> Result<Vec<OperationSpan>, String> {
    let mut out = Vec::new();
    let (mut i, mut start, mut depth) = (0, 0, 0i32);
    let mut fresh = true;
    let mut split_string = false;
    while i < data.len() {
        let c = data[i];
        if c.is_ascii_whitespace() || c == 0 {
            i += 1;
            continue;
        }
        if c == b'%' {
            while i < data.len() && !b"\r\n".contains(&data[i]) {
                i += 1;
            }
            continue;
        }
        if fresh {
            start = i;
            fresh = false;
        }
        if c == b'(' {
            let string_start = i;
            i += 1;
            let mut nesting = 1;
            while i < data.len() && nesting > 0 {
                match data[i] {
                    b'\\' => {
                        i += 1;
                        if i < data.len() && data[i] == b'\r' && data.get(i + 1) == Some(&b'\n') {
                            i += 1;
                        }
                    }
                    b'(' => nesting += 1,
                    b')' => nesting -= 1,
                    _ => (),
                }
                i += 1;
            }
            split_string |= boundaries.iter().any(|&b| string_start < b && b < i);
            if nesting != 0 {
                return Err("unterminated string".into());
            }
            continue;
        }
        if c == b'<' && data.get(i + 1) != Some(&b'<') {
            let string_start = i;
            i += 1;
            while i < data.len() && data[i] != b'>' {
                i += 1;
            }
            if i == data.len() {
                return Err("unterminated hex string".into());
            }
            i += 1;
            split_string |= boundaries.iter().any(|&b| string_start < b && b < i);
            continue;
        }
        if c == b'[' || (c == b'<' && data.get(i + 1) == Some(&b'<')) {
            depth += 1;
            i += if c == b'<' { 2 } else { 1 };
            continue;
        }
        if c == b']' || (c == b'>' && data.get(i + 1) == Some(&b'>')) {
            depth -= 1;
            if depth < 0 {
                return Err("unbalanced container".into());
            }
            i += if c == b'>' { 2 } else { 1 };
            continue;
        }
        let name = c == b'/';
        if name {
            i += 1;
        }
        let token = i;
        while i < data.len()
            && !data[i].is_ascii_whitespace()
            && !b"\0()<>[]{}/%".contains(&data[i])
        {
            i += 1;
        }
        if i == token {
            return Err("invalid token".into());
        }
        let word = &data[token..i];
        if depth != 0
            || name
            || word == b"true"
            || word == b"false"
            || word == b"null"
            || matches!(word[0], b'0'..=b'9' | b'.' | b'+' | b'-')
        {
            continue;
        }
        if word == b"BI" {
            return Err("inline-image".into());
        }
        let decoded =
            super::parser::operation(&data[start..i]).map_err(|_| "operation parse".to_owned())?;
        if decoded.operations.len() != 1 {
            return Err("operation boundary".into());
        }
        out.push((
            start..i,
            decoded.operations.into_iter().next().unwrap(),
            split_string,
        ));
        split_string = false;
        fresh = true;
    }
    if depth != 0 {
        return Err("unbalanced container".into());
    }
    Ok(out)
}

#[derive(Clone)]
struct State {
    ctm: Matrix,
    font: Vec<u8>,
    size: f64,
    tc: f64,
    tw: f64,
    h: f64,
    leading: f64,
    rise: f64,
    render: i64,
}
impl Default for State {
    fn default() -> Self {
        Self {
            ctm: ID,
            font: Vec::new(),
            size: 0.,
            tc: 0.,
            tw: 0.,
            h: 1.,
            leading: 0.,
            rise: 0.,
            render: 0,
        }
    }
}
struct Edit {
    range: Range<usize>,
    // Array entries and their matched character. Rejected paragraphs keep their strings.
    parts: Vec<(Object, Option<usize>, f64)>,
    prefix: String,
}

struct PlannedEdits {
    edits: Vec<Edit>,
    matches: Vec<usize>,
    split_paragraphs: HashSet<String>,
}

/// `blank` tells whether the glyph of a code in a font (by its resource name) paints
/// nothing; it is asked only of a space glyph that PDFium does not report.
fn edits(
    data: &[u8],
    fonts: &HashMap<Vec<u8>, Font>,
    targets: &[&EraseChar],
    safe_states: &HashSet<Vec<u8>>,
    all: &[&EraseChar],
    boundaries: &[usize],
    blank: &mut dyn FnMut(&[u8], &[u8]) -> bool,
) -> Result<PlannedEdits, String> {
    let mut state = State::default();
    let mut stack = Vec::new();
    let (mut tm, mut line) = (ID, ID);
    let mut edits = Vec::new();
    let mut matches = vec![0; targets.len()];
    let mut split_paragraphs = HashSet::new();
    for (range, op, split_string) in spans(data, boundaries)? {
        let a = &op.operands;
        let n = |i: usize| {
            a.get(i)
                .and_then(number)
                .ok_or_else(|| format!("{} operands", op.operator))
        };
        match op.operator.as_str() {
            "q" => stack.push(state.clone()),
            "Q" => state = stack.pop().ok_or("unbalanced Q")?,
            "cm" | "Tm" => {
                let m = [n(0)?, n(1)?, n(2)?, n(3)?, n(4)?, n(5)?];
                if op.operator == "cm" {
                    state.ctm = then(m, state.ctm);
                } else {
                    tm = m;
                    line = m;
                }
            }
            "BT" => {
                tm = ID;
                line = ID;
            }
            "Tf" => {
                state.font = a
                    .first()
                    .and_then(|o| o.as_name().ok())
                    .ok_or("Tf font")?
                    .to_vec();
                state.size = n(1)?;
            }
            "Tc" => state.tc = n(0)?,
            "Tw" => state.tw = n(0)?,
            "Tz" => state.h = n(0)? / 100.,
            "TL" => state.leading = n(0)?,
            "Ts" => state.rise = n(0)?,
            "Tr" => state.render = n(0)? as i64,
            "Td" | "TD" | "T*" => {
                let (x, y) = if op.operator == "T*" {
                    (0., -state.leading)
                } else {
                    (n(0)?, n(1)?)
                };
                if op.operator == "TD" {
                    state.leading = -y;
                }
                line = then([1., 0., 0., 1., x, y], line);
                tm = line;
            }
            "gs" => {
                let name = a.first().and_then(|o| o.as_name().ok()).ok_or("gs name")?;
                if !safe_states.contains(name) {
                    return Err("ExtGState font override or missing resource".into());
                }
            }
            "Tj" | "TJ" | "'" | "\"" => {
                let mut prefix = String::new();
                if op.operator == "\"" {
                    state.tw = n(0)?;
                    state.tc = n(1)?;
                    prefix = format!("{} Tw {} Tc ", state.tw, state.tc);
                }
                if op.operator == "'" || op.operator == "\"" {
                    line = then([1., 0., 0., 1., 0., -state.leading], line);
                    tm = line;
                    prefix.push_str("T* ");
                }
                let Some(font) = fonts.get(&state.font) else {
                    tm[4] = f64::NAN;
                    continue;
                };
                let last = a.last().ok_or("text operands")?;
                let values = if op.operator == "TJ" {
                    last.as_array().map_err(|_| "TJ array")?.as_slice()
                } else {
                    std::slice::from_ref(last)
                };
                let mut parts = Vec::new();
                let mut glyphs = Vec::new();
                for v in values {
                    if let Some(adjust) = number(v) {
                        tm = then(
                            [1., 0., 0., 1., -adjust * state.size * state.h / 1000., 0.],
                            tm,
                        );
                        parts.push((v.clone(), None, 0.));
                        glyphs.push(None);
                        continue;
                    }
                    let bytes = v.as_str().map_err(|_| "text string")?;
                    if bytes.len() % font.bytes != 0 {
                        return Err("incomplete character code".into());
                    }
                    for code in bytes.chunks(font.bytes) {
                        let cid = code.iter().fold(0u32, |v, b| v * 256 + u32::from(*b));
                        let Some(width) = font.widths.get(&cid).copied().or(font.default) else {
                            return Err("missing width".into());
                        };
                        let advance = width * state.size / 1000.
                            + state.tc
                            + if code == b" " { state.tw } else { 0. };
                        let pos = apply(then(tm, state.ctm), 0., state.rise);
                        let candidates: Vec<usize> = targets
                            .iter()
                            .enumerate()
                            .filter(|(_, t)| {
                                (t.origin.0 - pos.0).abs() < 0.15
                                    && (t.origin.1 - pos.1).abs() < 0.15
                            })
                            .map(|(i, _)| i)
                            .collect();
                        if split_string {
                            split_paragraphs
                                .extend(candidates.iter().map(|&i| targets[i].paragraph.clone()));
                        }
                        let unicode_agrees = !candidates.is_empty()
                            && candidates
                                .iter()
                                .all(|&i| targets[i].paragraph == targets[candidates[0]].paragraph)
                            && font.unicode.get(&cid).is_some_and(|codes| {
                                codes.len() == candidates.len()
                                    && codes.iter().zip(&candidates).all(|(&code, &i)| {
                                        inplace::matches_source_unicode(targets[i], code)
                                    })
                            });
                        let overlaps_unselected = all.iter().any(|c| {
                            (c.origin.0 - pos.0).abs() < 0.15
                                && (c.origin.1 - pos.1).abs() < 0.15
                                && !candidates.iter().any(|&i| targets[i].index == c.index)
                        });
                        let target = if unicode_agrees
                            && !overlaps_unselected
                            && !split_string
                            && state.size > 0.
                            && state.render < 4
                        {
                            for &i in &candidates {
                                matches[i] += 1;
                            }
                            Some(candidates[0])
                        } else {
                            None
                        };
                        // PDFium does not report a space glyph that follows another space.
                        // Once its neighbour is erased it becomes a reported character, and
                        // the post-erasure check rejects the page. Such a glyph has no
                        // character of its own at its origin.
                        let unreported_space = candidates.is_empty()
                            && !overlaps_unselected
                            && !split_string
                            && state.size > 0.
                            && state.render < 4
                            && font
                                .unicode
                                .get(&cid)
                                .is_some_and(|u| u.as_slice() == [0x20]);
                        parts.push((
                            Object::String(code.to_vec(), StringFormat::Hexadecimal),
                            target,
                            -advance * 1000. / state.size,
                        ));
                        tm = then([1., 0., 0., 1., advance * state.h, 0.], tm);
                        let m = then(tm, state.ctm);
                        glyphs.push(Some(Placed {
                            start: pos,
                            end: apply(m, 0., state.rise),
                            em: state.size * m[2].hypot(m[3]),
                            space: unreported_space.then(|| code.to_vec()),
                        }));
                    }
                }
                // A synthetic delimiter inside a string would change its width. No position
                // derived from that string is trustworthy until a new absolute text matrix.
                if split_string {
                    tm[4] = f64::NAN;
                }
                absorb_unreported_spaces(&mut parts, &glyphs, targets, &mut |code| {
                    blank(&state.font, code)
                });
                if parts.iter().any(|(_, t, _)| t.is_some()) {
                    edits.push(Edit {
                        range,
                        parts,
                        prefix,
                    });
                }
            }
            _ => (),
        }
    }
    Ok(PlannedEdits {
        edits,
        matches,
        split_paragraphs,
    })
}

/// Where a glyph of a text-showing operation is drawn, in PDF user space: its origin,
/// the origin after its advance, and the font size there; and its code if it is a space
/// glyph that PDFium does not report.
struct Placed {
    start: (f64, f64),
    end: (f64, f64),
    em: f64,
    space: Option<Vec<u8>>,
}

/// An unreported space glyph goes with the erased glyph directly before it in the same
/// text-showing operation (numeric adjustments aside), or else directly after it, and only
/// when it is next to that glyph on the page and paints nothing:
///
/// - the gap between them is at most the space's own advance (one space), whatever the
///   array's adjustments (a `TJ` can move a glyph anywhere on the line);
/// - it lies in its paragraph's box (of the origins of the paragraph's characters on the
///   page, one em around);
/// - `blank` has its glyph painting nothing (a font program embedded and read, its outline
///   empty); a font that cannot tell keeps it.
///
/// It is removed only if that glyph's paragraph is: matching counts are not changed.
fn absorb_unreported_spaces(
    parts: &mut [(Object, Option<usize>, f64)],
    glyphs: &[Option<Placed>],
    targets: &[&EraseChar],
    blank: &mut dyn FnMut(&[u8]) -> bool,
) {
    let distance = |a: (f64, f64), b: (f64, f64)| (a.0 - b.0).hypot(a.1 - b.1);
    let in_box = |paragraph: &str, g: &Placed| {
        let origins = targets
            .iter()
            .filter(|t| t.paragraph == paragraph)
            .map(|t| t.origin);
        let (l, b, r, t) = origins.fold(
            (
                f64::INFINITY,
                f64::INFINITY,
                f64::NEG_INFINITY,
                f64::NEG_INFINITY,
            ),
            |(l, b, r, t), (x, y)| (l.min(x), b.min(y), r.max(x), t.max(y)),
        );
        [g.start, g.end]
            .iter()
            .all(|&(x, y)| x >= l - g.em && x <= r + g.em && y >= b - g.em && y <= t + g.em)
    };
    let mut takes = |parts: &[(Object, Option<usize>, f64)], k: usize, j: usize| {
        let (Some(g), Some(n), Some(i)) = (&glyphs[k], &glyphs[j], parts[j].1) else {
            return None;
        };
        let code = g.space.as_ref()?;
        let gap = if j < k {
            distance(n.end, g.start)
        } else {
            distance(g.end, n.start)
        };
        (gap <= distance(g.start, g.end) + 0.15 && in_box(&targets[i].paragraph, g) && blank(code))
            .then_some(i)
    };
    let glyph = |k: usize| glyphs[k].is_some();
    for k in 0..parts.len() {
        if let Some(j) = (0..k).rev().find(|&j| glyph(j))
            && let Some(i) = takes(parts, k, j)
        {
            parts[k].1 = Some(i);
        }
    }
    for k in (0..parts.len()).rev() {
        if parts[k].1.is_none()
            && let Some(j) = (k + 1..parts.len()).find(|&j| glyph(j))
            && let Some(i) = takes(parts, k, j)
        {
            parts[k].1 = Some(i);
        }
    }
}

/// Whether the glyph of `code` in a font paints nothing, as PDFium draws it alone, large,
/// filled and stroked, from the font program embedded in the file. A font without one
/// (not embedded, Type 3, or a program PDFium cannot read and replaces) cannot tell.
fn paints_nothing(doc: &Document, font: &Dictionary, code: &[u8]) -> bool {
    use lopdf::dictionary;
    let embedded = |d: &Dictionary| {
        field(doc, d, b"FontDescriptor")
            .and_then(|o| o.as_dict().ok())
            .is_some_and(|f| {
                [&b"FontFile"[..], b"FontFile2", b"FontFile3"]
                    .iter()
                    .any(|k| f.has(k))
            })
    };
    let program = match font.get(b"Subtype").and_then(Object::as_name) {
        Ok(b"TrueType" | b"Type1") => embedded(font),
        Ok(b"Type0") => field(doc, font, b"DescendantFonts")
            .and_then(|o| o.as_array().ok())
            .and_then(|a| a.first())
            .and_then(|o| resolved(doc, o))
            .and_then(|o| o.as_dict().ok())
            .is_some_and(embedded),
        _ => false,
    };
    if !program {
        return false;
    }
    let mut probe = Document::with_version("1.7");
    let mut ids = HashMap::new();
    let mut budget = 64;
    let Some(font) = copy_object(
        doc,
        &Object::Dictionary(font.clone()),
        &mut probe,
        &mut ids,
        &mut budget,
    ) else {
        return false;
    };
    let hex: String = code.iter().map(|b| format!("{b:02X}")).collect();
    // 50 pt glyphs, 10 em each way from their origin on a 1000 pt page.
    let content = format!("BT /F0 50 Tf 2 Tr 1 w 0 g 0 G 1 0 0 1 500 500 Tm <{hex}> Tj ET");
    let contents = probe.add_object(lopdf::Stream::new(Dictionary::new(), content.into_bytes()));
    let pages = probe.new_object_id();
    let page = probe.add_object(dictionary! {
        "Type" => "Page", "Parent" => pages,
        "MediaBox" => vec![0.into(), 0.into(), 1000.into(), 1000.into()],
        "Resources" => dictionary! { "Font" => dictionary! { "F0" => font } },
        "Contents" => contents,
    });
    probe.objects.insert(
        pages,
        Object::Dictionary(
            dictionary! { "Type" => "Pages", "Kids" => vec![page.into()], "Count" => 1 },
        ),
    );
    let root = probe.add_object(dictionary! { "Type" => "Catalog", "Pages" => pages });
    probe.trailer.set("Root", root);
    let mut bytes = Vec::new();
    probe.save_to(&mut bytes).is_ok() && inplace::paints_nothing(&bytes).unwrap_or(false)
}

/// `object` of `from` with what it refers to (at most `budget` objects), into `to`.
fn copy_object(
    from: &Document,
    object: &Object,
    to: &mut Document,
    ids: &mut HashMap<lopdf::ObjectId, lopdf::ObjectId>,
    budget: &mut usize,
) -> Option<Object> {
    let copy_dict =
        |d: &Dictionary, to: &mut Document, ids: &mut HashMap<_, _>, budget: &mut usize| {
            let mut out = Dictionary::new();
            for (k, v) in d.iter() {
                out.set(k.clone(), copy_object(from, v, to, ids, budget)?);
            }
            Some(out)
        };
    Some(match object {
        Object::Reference(id) => {
            if let Some(&new) = ids.get(id) {
                return Some(Object::Reference(new));
            }
            *budget = budget.checked_sub(1)?;
            let new = to.new_object_id();
            ids.insert(*id, new);
            let inner = copy_object(from, from.get_object(*id).ok()?, to, ids, budget)?;
            to.objects.insert(new, inner);
            Object::Reference(new)
        }
        Object::Array(a) => Object::Array(
            a.iter()
                .map(|v| copy_object(from, v, to, ids, budget))
                .collect::<Option<_>>()?,
        ),
        Object::Dictionary(d) => Object::Dictionary(copy_dict(d, to, ids, budget)?),
        Object::Stream(s) => {
            let mut stream =
                lopdf::Stream::new(copy_dict(&s.dict, to, ids, budget)?, s.content.clone());
            stream.allows_compression = false;
            Object::Stream(stream)
        }
        other => other.clone(),
    })
}

/// Whether glyphs paint nothing ([`paints_nothing`]), by font and code, for one source: the
/// erasures of one save (each replan erases from the same original again) draw each glyph
/// of a font once. Given another source, it starts over.
#[derive(Default)]
pub struct Blanks {
    source: Option<u64>,
    known: HashMap<(FontKey, Vec<u8>), bool>,
    /// How many glyphs were drawn to tell.
    drawn: usize,
}

/// A font of the source: its own object, or one written in a page's resources (that page
/// and its name there).
#[derive(Clone, Debug, PartialEq, Eq, Hash)]
enum FontKey {
    Object(lopdf::ObjectId),
    Inline(lopdf::ObjectId, Vec<u8>),
}

impl Blanks {
    fn of(&mut self, bytes: &[u8]) -> &mut Self {
        use std::hash::{Hash, Hasher};
        let mut hasher = std::collections::hash_map::DefaultHasher::new();
        bytes.hash(&mut hasher);
        let source = hasher.finish();
        if self.source != Some(source) {
            *self = Self {
                source: Some(source),
                ..Self::default()
            };
        }
        self
    }

    fn paints_nothing(&mut self, font: FontKey, code: &[u8], draw: impl FnOnce() -> bool) -> bool {
        let drawn = &mut self.drawn;
        *self.known.entry((font, code.to_vec())).or_insert_with(|| {
            *drawn += 1;
            draw()
        })
    }
}

/// Edit only page contents; clone edited streams to avoid affecting shared page resources.
/// Unsupported or ambiguous characters retain their entire paragraph (no partial erasure).
pub fn erase(bytes: &[u8], targets: &[EraseChar]) -> Result<(Vec<u8>, Report), Error> {
    erase_with(bytes, targets, &mut Blanks::default())
}

/// [`erase`], with what is known of the source's glyphs from its earlier erasures.
pub fn erase_with(
    bytes: &[u8],
    targets: &[EraseChar],
    blanks: &mut Blanks,
) -> Result<(Vec<u8>, Report), Error> {
    let blanks = blanks.of(bytes);
    super::parser::with_document(bytes, |doc| erase_document(bytes, targets, doc, blanks))
}

/// The objects of `doc`'s font dictionaries, by where they are (as `get_page_fonts` lends
/// them).
fn font_objects(doc: &Document) -> HashMap<usize, lopdf::ObjectId> {
    doc.objects
        .iter()
        .filter_map(|(id, o)| {
            let d = o.as_dict().ok()?;
            (d.get(b"Type").and_then(Object::as_name).ok() == Some(&b"Font"[..]))
                .then_some((std::ptr::from_ref(d) as usize, *id))
        })
        .collect()
}

fn erase_document(
    bytes: &[u8],
    targets: &[EraseChar],
    doc: Document,
    blanks: &mut Blanks,
) -> Result<(Vec<u8>, Report), Error> {
    validate_trailer_size(&doc)?;
    let font_objects = font_objects(&doc);
    let pages = targets.iter().map(|c| c.page).collect();
    let original_chars = inplace::characters_on_pages(bytes, None, Some(&pages))?;
    let mut incremental = IncrementalDocument::create_from(bytes.to_vec(), doc.clone());
    incremental.new_document.version = doc.version.clone();
    incremental.new_document.max_id = doc.max_id.max(trailer_size(&doc)? - 1);
    let mut glyphs = Glyphs {
        objects: &font_objects,
        blanks,
    };
    let (mut out, mut report) = erase_attempt(
        bytes,
        &doc,
        &original_chars,
        targets,
        &mut incremental,
        &mut glyphs,
    )?;
    let failed = report.retained.clone();
    for (id, reason) in &failed {
        if reason != "post-erasure-character-or-origin-mismatch" {
            continue;
        }
        let page = targets.iter().find(|c| &c.paragraph == id).unwrap().page;
        let selected: Vec<_> = targets
            .iter()
            .filter(|c| {
                c.page == page && (report.erased.contains(&c.paragraph) || &c.paragraph == id)
            })
            .cloned()
            .collect();
        // Only this failed page is rebuilt from original content. Verified page
        // objects and streams in the incremental revision are reused verbatim.
        let mut trial = incremental.clone();
        let (candidate, checked) = erase_attempt(
            bytes,
            &doc,
            &original_chars,
            &selected,
            &mut trial,
            &mut glyphs,
        )?;
        if checked.erased.contains(id)
            && selected.iter().all(|c| {
                !report.erased.contains(&c.paragraph) || checked.erased.contains(&c.paragraph)
            })
        {
            let old_count = selected
                .iter()
                .filter(|c| report.erased.contains(&c.paragraph))
                .count();
            report.matched_characters =
                report.matched_characters - old_count + checked.matched_characters;
            for accepted in checked.erased {
                if !report.erased.contains(&accepted) {
                    report.erased.push(accepted);
                }
            }
            report.retained.remove(id);
            report
                .diagnostics
                .push(format!("page {}: replanned isolated erasure", page + 1));
            incremental = trial;
            out = candidate;
        }
    }
    report.targeted_characters = targets.len();
    Ok((out, report))
}

#[cfg(test)]
fn erase_once(bytes: &[u8], targets: &[EraseChar]) -> Result<(Vec<u8>, Report), Error> {
    let doc = Document::load_mem(bytes)?;
    let original_chars = inplace::characters(bytes, None)?;
    let mut incremental = IncrementalDocument::create_from(bytes.to_vec(), doc.clone());
    incremental.new_document.max_id = doc.max_id.max(trailer_size(&doc)? - 1);
    let mut glyphs = Glyphs {
        objects: &font_objects(&doc),
        blanks: &mut Blanks::default(),
    };
    erase_attempt(
        bytes,
        &doc,
        &original_chars,
        targets,
        &mut incremental,
        &mut glyphs,
    )
}

/// What an erasure attempt asks of its source's glyphs: the fonts' objects
/// ([`font_objects`]) and what is known of them ([`Blanks`]).
struct Glyphs<'a> {
    objects: &'a HashMap<usize, lopdf::ObjectId>,
    blanks: &'a mut Blanks,
}

fn erase_attempt(
    bytes: &[u8],
    doc: &Document,
    original_chars: &[EraseChar],
    targets: &[EraseChar],
    incremental: &mut IncrementalDocument,
    glyphs: &mut Glyphs<'_>,
) -> Result<(Vec<u8>, Report), Error> {
    let mut changed_pages = BTreeMap::new();
    let mut report = Report {
        targeted_characters: targets.len(),
        ..Report::default()
    };
    for (page_number, page_id) in doc.get_pages() {
        let here: Vec<_> = targets
            .iter()
            .filter(|t| t.page + 1 == page_number as usize)
            .collect();
        if here.is_empty() {
            continue;
        }
        let page_fonts = doc.get_page_fonts(page_id)?;
        let mut blank = |name: &[u8], code: &[u8]| {
            let Some(&d) = page_fonts.get(name) else {
                return false;
            };
            let font = glyphs
                .objects
                .get(&(std::ptr::from_ref(d) as usize))
                .map_or_else(
                    || FontKey::Inline(page_id, name.to_vec()),
                    |&id| FontKey::Object(id),
                );
            glyphs
                .blanks
                .paints_nothing(font, code, || paints_nothing(doc, d, code))
        };
        let fonts: HashMap<_, _> = page_fonts
            .iter()
            .map(|(name, d)| (name.clone(), *d))
            .filter_map(|(name, d)| {
                let f = Font::read(doc, d);
                if f.is_none() {
                    report.diagnostics.push(format!(
                        "page {page_number}: unsupported font {}",
                        String::from_utf8_lossy(&name)
                    ));
                }
                f.map(|f| (name, f))
            })
            .collect();
        let mut safe_states = HashSet::new();
        let (own, inherited) = doc.get_page_resources(page_id)?;
        let resources: Vec<_> = own
            .into_iter()
            .chain(
                inherited
                    .iter()
                    .filter_map(|id| doc.get_object(*id).ok()?.as_dict().ok()),
            )
            .collect();
        let mut seen_states = HashSet::new();
        for r in resources {
            if let Some(states) = field(doc, r, b"ExtGState").and_then(|o| o.as_dict().ok()) {
                for (name, state) in states.iter() {
                    if !seen_states.insert(name.clone()) {
                        continue;
                    }
                    if let Some(d) = resolved(doc, state).and_then(|o| o.as_dict().ok())
                        && !d.has(b"Font")
                    {
                        safe_states.insert(name.clone());
                    }
                }
            }
        }
        let ids = doc.get_page_contents(page_id);
        let mut data = Vec::new();
        let mut ranges = Vec::new();
        for id in &ids {
            let s = doc.get_object(*id)?.as_stream()?;
            let content = s.get_plain_content_with_limit(super::parser::STREAM_LIMIT)?;
            let start = data.len();
            data.extend(content);
            ranges.push(start..data.len());
            data.push(b'\n');
        }
        let all: Vec<_> = original_chars
            .iter()
            .filter(|c| c.page + 1 == page_number as usize)
            .collect();
        let boundaries: Vec<_> = ranges
            .iter()
            .take(ranges.len().saturating_sub(1))
            .map(|r| r.end)
            .collect();
        let PlannedEdits {
            edits,
            matches,
            split_paragraphs,
        } = match edits(
            &data,
            &fonts,
            &here,
            &safe_states,
            &all,
            &boundaries,
            &mut blank,
        ) {
            Ok(r) => r,
            Err(reason) => {
                report
                    .diagnostics
                    .push(format!("page {page_number}: {reason}"));
                for c in here {
                    report.retained.insert(c.paragraph.clone(), reason.clone());
                }
                continue;
            }
        };
        let matched_before_page = report.matched_characters;
        let mut rejected = HashSet::new();
        for (i, c) in here.iter().enumerate() {
            if matches[i] != 1
                || !all.iter().any(|original| {
                    original.index == c.index
                        && original.is_hyphen == c.is_hyphen
                        && inplace::same_character(original, c)
                })
            {
                rejected.insert(c.paragraph.clone());
            } else {
                report.matched_characters += 1;
            }
        }
        for c in &here {
            if rejected.contains(&c.paragraph) {
                report.retained.insert(
                    c.paragraph.clone(),
                    if split_paragraphs.contains(&c.paragraph) {
                        "string-crosses-content-stream-boundary"
                    } else {
                        "unmatched-or-ambiguous-character"
                    }
                    .into(),
                );
            } else if !report.erased.contains(&c.paragraph) {
                report.erased.push(c.paragraph.clone());
            }
        }
        let mut replacements = Vec::new();
        for e in edits {
            if !e
                .parts
                .iter()
                .any(|(_, t, _)| t.is_some_and(|i| !rejected.contains(&here[i].paragraph)))
            {
                continue;
            }
            let mut array = Vec::new();
            for (obj, t, advance) in e.parts {
                if t.is_some_and(|i| !rejected.contains(&here[i].paragraph)) {
                    array.push(Object::Real(advance as f32));
                } else {
                    array.push(obj);
                }
            }
            let replacement = Content {
                operations: vec![lopdf::content::Operation::new(
                    "TJ",
                    vec![Object::Array(array)],
                )],
            }
            .encode()?;
            let start = e.range.start
                + data[e.range.clone()]
                    .iter()
                    .take_while(|b| b.is_ascii_whitespace())
                    .count();
            let mut value = e.prefix.into_bytes();
            value.extend(replacement);
            replacements.push((start..e.range.end, value));
        }
        if replacements.is_empty() {
            continue;
        }
        let mut allocated = vec![page_id];
        let mut new_ids = Vec::new();
        for (id, range) in ids.iter().zip(ranges) {
            let mut content = data[range.clone()].to_vec();
            let mut changed = false;
            for (r, v) in replacements.iter().rev() {
                let start = r.start.max(range.start);
                let end = r.end.min(range.end);
                if start < end {
                    // An operation can cross /Contents streams. Keep the stream boundaries,
                    // put its replacement at its original start, and remove only its tail.
                    let inserted = if r.start >= range.start {
                        v.as_slice()
                    } else {
                        &[]
                    };
                    content.splice(
                        start - range.start..end - range.start,
                        inserted.iter().copied(),
                    );
                    changed = true;
                }
            }
            let new_id = if changed {
                let mut stream = doc.get_object(*id)?.as_stream()?.clone();
                stream.set_plain_content(content);
                stream.compress()?;
                let id = incremental.new_document.add_object(stream);
                allocated.push(id);
                id
            } else {
                *id
            };
            new_ids.push(Object::Reference(new_id));
        }
        incremental.opt_clone_object_to_new_document(page_id)?;
        incremental
            .new_document
            .get_object_mut(page_id)?
            .as_dict_mut()?
            .set("Contents", Object::Array(new_ids));
        changed_pages.insert(
            page_number as usize - 1,
            (allocated, report.matched_characters - matched_before_page),
        );
    }
    if changed_pages.is_empty() {
        return Ok((bytes.to_vec(), report));
    }
    let mut out = save_incremental(incremental)?;
    if let Err(error) = validate_output(&out) {
        return Ok(restore_original(bytes, targets, report, error));
    }
    let pages = changed_pages.keys().copied().collect();
    let after = inplace::characters_on_pages(&out, None, Some(&pages))?;
    let selected: HashSet<_> = targets
        .iter()
        .filter(|c| report.erased.contains(&c.paragraph))
        .map(|c| (c.page, c.index))
        .collect();
    let mut rolled_back = false;
    for (page, (allocated, matched)) in changed_pages {
        let expected = original_chars
            .iter()
            .filter(|c| c.page == page && !selected.contains(&(page, c.index)));
        let actual = after.iter().filter(|c| c.page == page);
        if !inplace::same_characters(expected, actual) {
            for id in allocated {
                incremental.new_document.objects.remove(&id);
            }
            report.matched_characters -= matched;
            for c in targets.iter().filter(|c| c.page == page) {
                report.erased.retain(|id| id != &c.paragraph);
                report
                    .retained
                    .entry(c.paragraph.clone())
                    .or_insert_with(|| "post-erasure-character-or-origin-mismatch".into());
            }
            report.diagnostics.push(format!(
                "page {}: post-erasure validation failed; page restored",
                page + 1
            ));
            rolled_back = true;
        }
    }
    if rolled_back {
        out = if report.erased.is_empty() {
            bytes.to_vec()
        } else {
            save_incremental(incremental)?
        };
    }
    if rolled_back && let Err(error) = validate_output(&out) {
        return Ok(restore_original(bytes, targets, report, error));
    }
    Ok((out, report))
}

/// Moves each link of `moves` ([`inplace::followed_links`]) onto its translation: its
/// `/Rect`, and its one quadrilateral if it has `/QuadPoints`. Only the one link annotation of
/// the page at that rectangle, listed by no other page, without an appearance of its own
/// (nothing drawn moves), that goes to a URL (a `/URI` action, no `/Dest`); any other stays
/// where it is. Returns the bytes (an incremental
/// update, or the same bytes) and the pages (from 0) of the links moved.
pub(super) fn move_links(
    bytes: &[u8],
    moves: &[inplace::FollowedLink],
) -> Result<(Vec<u8>, Vec<usize>), Error> {
    if moves.is_empty() {
        return Ok((bytes.to_vec(), Vec::new()));
    }
    super::parser::with_document(bytes, |doc| move_document_links(bytes, moves, doc))
}

fn move_document_links(
    bytes: &[u8],
    moves: &[inplace::FollowedLink],
    doc: Document,
) -> Result<(Vec<u8>, Vec<usize>), Error> {
    let pages = doc.get_pages();
    let annotations = |page: lopdf::ObjectId| -> Vec<lopdf::ObjectId> {
        doc.get_dictionary(page)
            .ok()
            .and_then(|d| field(&doc, d, b"Annots"))
            .and_then(|o| o.as_array().ok())
            .map(|a| a.iter().filter_map(|o| o.as_reference().ok()).collect())
            .unwrap_or_default()
    };
    let mut listed: HashMap<lopdf::ObjectId, usize> = HashMap::new();
    for page in pages.values() {
        for id in annotations(*page) {
            *listed.entry(id).or_default() += 1;
        }
    }
    let numbers = |o: Option<&Object>| -> Option<Vec<f64>> {
        o?.as_array()
            .ok()?
            .iter()
            .map(|v| resolved(&doc, v).and_then(number))
            .collect()
    };
    let mut incremental = IncrementalDocument::create_from(bytes.to_vec(), doc.clone());
    incremental.new_document.version = doc.version.clone();
    incremental.new_document.max_id = doc.max_id.max(trailer_size(&doc)? - 1);
    let mut moved = Vec::new();
    for m in moves {
        let Some(&page) = pages.get(&(m.page as u32 + 1)) else {
            continue;
        };
        let at: Vec<_> = annotations(page)
            .into_iter()
            .filter(|id| {
                doc.get_dictionary(*id).is_ok_and(|d| {
                    d.get(b"Subtype").and_then(Object::as_name).ok() == Some(&b"Link"[..])
                        && numbers(field(&doc, d, b"Rect")).is_some_and(|r| {
                            r.len() == 4
                                && [
                                    r[0].min(r[2]),
                                    r[1].min(r[3]),
                                    r[0].max(r[2]),
                                    r[1].max(r[3]),
                                ]
                                .iter()
                                .zip(m.link)
                                .all(|(a, b)| (a - b).abs() < 0.05)
                        })
                })
            })
            .collect();
        let [id] = at[..] else {
            continue;
        };
        let Ok(d) = doc.get_dictionary(id) else {
            continue;
        };
        let quad = match d.get(b"QuadPoints") {
            Err(_) => false,
            Ok(_) if numbers(field(&doc, d, b"QuadPoints")).is_some_and(|q| q.len() == 8) => true,
            Ok(_) => continue,
        };
        // Only a link out to a URL: one to a page or a named place of the document (a
        // citation's number, a footnote's, a contents line) points at those words alone.
        let to_url = !d.has(b"Dest")
            && field(&doc, d, b"A")
                .and_then(|o| o.as_dict().ok())
                .is_some_and(|a| a.get(b"S").and_then(Object::as_name).ok() == Some(&b"URI"[..]));
        if listed.get(&id) != Some(&1) || d.has(b"AP") || !to_url {
            continue;
        }
        incremental.opt_clone_object_to_new_document(id)?;
        let [l, b, r, t] = m.drawn;
        let reals = |v: &[f64]| Object::Array(v.iter().map(|&x| Object::Real(x as f32)).collect());
        let link = incremental.new_document.get_object_mut(id)?.as_dict_mut()?;
        link.set("Rect", reals(&[l, b, r, t]));
        if quad {
            link.set("QuadPoints", reals(&[l, t, r, t, l, b, r, b]));
        }
        moved.push(m.page);
    }
    if moved.is_empty() {
        return Ok((bytes.to_vec(), moved));
    }
    let out = save_incremental(&incremental)?;
    validate_output(&out)?;
    Ok((out, moved))
}

fn restore_original(
    bytes: &[u8],
    targets: &[EraseChar],
    mut report: Report,
    error: Error,
) -> (Vec<u8>, Report) {
    report.erased.clear();
    report.matched_characters = 0;
    for c in targets {
        report
            .retained
            .entry(c.paragraph.clone())
            .or_insert_with(|| "post-erasure-structure-mismatch".into());
    }
    report.diagnostics.push(format!(
        "post-erasure structure validation failed; original restored: {error}"
    ));
    (bytes.to_vec(), report)
}

fn trailer_size(doc: &Document) -> Result<u32, Error> {
    let size = doc.trailer.get(b"Size")?.as_i64()?;
    u32::try_from(size)
        .ok()
        .filter(|&n| n > 0 && n < u32::MAX)
        .ok_or_else(|| Error::IncompleteObjects("invalid trailer Size".into()))
}

fn validate_output(bytes: &[u8]) -> Result<(), Error> {
    super::parser::with_document(bytes, |doc| validate_trailer_size(&doc))
}

fn validate_trailer_size(doc: &Document) -> Result<(), Error> {
    if trailer_size(doc)? <= doc.reference_table.max_id() {
        return Err(Error::IncompleteObjects(
            "trailer Size is below maximum object number + 1".into(),
        ));
    }
    Ok(())
}

fn save_incremental(doc: &IncrementalDocument) -> Result<Vec<u8>, Error> {
    // The writer mutates xref metadata, so each attempt starts with a fresh revision.
    let mut revision = doc.clone();
    // Remove abandoned new IDs after rollback, but preserve the original free slots.
    revision.new_document.max_id = revision
        .new_document
        .objects
        .keys()
        .next_back()
        .map_or(doc.get_prev_documents().max_id, |id| {
            id.0.max(doc.get_prev_documents().max_id)
        });
    revision.new_document.max_id = revision
        .new_document
        .max_id
        .max(trailer_size(doc.get_prev_documents())? - 1);
    let mut out = Vec::new();
    revision.save_to(&mut out)?;
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::pdf::layout::Rect;
    use lopdf::{Stream, dictionary};

    fn fixture(streams: &[&[u8]]) -> Vec<u8> {
        fixture_with(Dictionary::new(), streams)
    }

    /// Helvetica with 500-unit widths; `extra` adds or replaces font dictionary entries.
    fn fixture_with(extra: Dictionary, streams: &[&[u8]]) -> Vec<u8> {
        let mut doc = Document::with_version("1.7");
        let pages = doc.new_object_id();
        let mut font = dictionary! { "Type"=>"Font", "Subtype"=>"Type1", "BaseFont"=>"Helvetica",
        "FirstChar"=>0, "LastChar"=>255, "Widths"=>Object::Array(vec![Object::Integer(500);256]) };
        for (key, value) in extra.iter() {
            font.set(key.clone(), indirect(&mut doc, value));
        }
        let font = doc.add_object(font);
        let resources = doc.add_object(dictionary! { "Font"=>dictionary! {"F1"=>font} });
        let contents: Vec<Object> = streams
            .iter()
            .map(|s| Object::Reference(doc.add_object(Stream::new(Dictionary::new(), s.to_vec()))))
            .collect();
        let page = doc.add_object(dictionary! {"Type"=>"Page", "Parent"=>pages, "MediaBox"=>vec![0.into(),0.into(),600.into(),800.into()], "Resources"=>resources,"Contents"=>contents });
        doc.objects.insert(
            pages,
            Object::Dictionary(dictionary! {"Type"=>"Pages","Kids"=>vec![page.into()],"Count"=>1}),
        );
        let root = doc.add_object(dictionary! {"Type"=>"Catalog","Pages"=>pages});
        doc.trailer.set("Root", root);
        let mut out = Vec::new();
        doc.save_to(&mut out).unwrap();
        out
    }
    /// `value` with every stream in it made an indirect object of `doc`.
    fn indirect(doc: &mut Document, value: &Object) -> Object {
        match value {
            Object::Stream(s) => Object::Reference(doc.add_object(s.clone())),
            Object::Dictionary(d) => {
                let mut out = Dictionary::new();
                for (k, v) in d.iter() {
                    out.set(k.clone(), indirect(doc, v));
                }
                Object::Dictionary(out)
            }
            other => other.clone(),
        }
    }

    /// A TrueType font program of 1000 units to the em: no outline for `.notdef`, a 250
    /// unit space with `space` (a box `[x0, y0, x1, y1]`) as its outline if any, and for
    /// each of `!` to `~` a 500 unit glyph, a box from 50, 0 to 450, 700.
    pub(crate) fn test_truetype(space: Option<[i16; 4]>) -> Vec<u8> {
        fn boxed([x0, y0, x1, y1]: [i16; 4]) -> Vec<u8> {
            let mut g = Vec::new();
            for v in [1, x0, y0, x1, y1] {
                g.extend(v.to_be_bytes());
            }
            g.extend(3u16.to_be_bytes()); // the last point of the one contour
            g.extend(0u16.to_be_bytes()); // no instructions
            g.extend([1u8; 4]); // four points on the outline, coordinates as 16-bit deltas
            for d in [x0, x1 - x0, 0, x0 - x1, y0, 0, y1 - y0, 0] {
                g.extend(d.to_be_bytes());
            }
            g
        }
        let mut glyphs = vec![(500, None), (250, space)];
        glyphs.extend((0x21..=0x7e).map(|_| (500, Some([50, 0, 450, 700]))));
        let n = glyphs.len() as u16;
        let (mut glyf, mut loca, mut hmtx) = (Vec::new(), Vec::new(), Vec::new());
        for (advance, outline) in &glyphs {
            loca.extend((glyf.len() as u32).to_be_bytes());
            glyf.extend(outline.map(boxed).unwrap_or_default());
            hmtx.extend((*advance as u16).to_be_bytes());
            hmtx.extend(outline.map_or(0, |o| o[0]).to_be_bytes());
        }
        loca.extend((glyf.len() as u32).to_be_bytes());
        let be = |values: &[i64], sizes: &[usize]| -> Vec<u8> {
            values
                .iter()
                .zip(sizes)
                .flat_map(|(v, s)| v.to_be_bytes()[8 - s..].to_vec())
                .collect()
        };
        let head = be(
            &[
                0x10000, 0x10000, 0, 0x5F0F3CF5, 0, 1000, 0, 0, 0, -200, 500, 800, 0, 8, 2, 1, 0,
            ],
            &[4, 4, 4, 4, 2, 2, 8, 8, 2, 2, 2, 2, 2, 2, 2, 2, 2],
        );
        let hhea = be(
            &[
                0x10000,
                800,
                -200,
                0,
                500,
                0,
                0,
                450,
                1,
                0,
                0,
                0,
                0,
                0,
                0,
                0,
                i64::from(n),
            ],
            &[4, 2, 2, 2, 2, 2, 2, 2, 2, 2, 2, 2, 2, 2, 2, 2, 2],
        );
        let mut maxp = be(
            &[0x10000, i64::from(n), 4, 1, 0, 0, 2],
            &[4, 2, 2, 2, 2, 2, 2],
        );
        maxp.extend([0u8; 16]);
        // Format 4, (3, 1): the space to glyph 1, `!` to `~` to glyphs 2 to 95.
        let ends = [0x20i64, 0x7e, 0xffff];
        let starts = [0x20i64, 0x21, 0xffff];
        let deltas = [1 - 0x20i64, 2 - 0x21, 1];
        let mut sub = be(&[4, 16 + 8 * 3, 0, 6, 4, 1, 2], &[2; 7]);
        for v in ends {
            sub.extend(be(&[v], &[2]));
        }
        sub.extend([0, 0]);
        for v in starts {
            sub.extend(be(&[v], &[2]));
        }
        for v in deltas {
            sub.extend(((v & 0xffff) as u16).to_be_bytes());
        }
        sub.extend([0u8; 6]); // no range offsets
        let mut cmap = be(&[0, 1, 3, 1, 12], &[2, 2, 2, 2, 4]);
        cmap.extend(sub);
        let tables: [(&[u8; 4], Vec<u8>); 7] = [
            (b"cmap", cmap),
            (b"glyf", glyf),
            (b"head", head),
            (b"hhea", hhea),
            (b"hmtx", hmtx),
            (b"loca", loca),
            (b"maxp", maxp),
        ];
        let mut out = be(&[0x10000, 7, 64, 2, 48], &[4, 2, 2, 2, 2]);
        let mut offset = 12 + 16 * tables.len();
        let mut data = Vec::new();
        for (tag, t) in &tables {
            let mut padded = t.clone();
            padded.resize(t.len().div_ceil(4) * 4, 0);
            let sum = padded
                .chunks(4)
                .map(|c| u32::from_be_bytes([c[0], c[1], c[2], c[3]]))
                .fold(0u32, u32::wrapping_add);
            out.extend(*tag);
            out.extend(sum.to_be_bytes());
            out.extend((offset as u32).to_be_bytes());
            out.extend((t.len() as u32).to_be_bytes());
            offset += padded.len();
            data.extend(padded);
        }
        out.extend(data);
        out
    }

    /// The entries that make [`fixture_with`]'s font [`test_truetype`], embedded.
    fn embedded(space: Option<[i16; 4]>) -> Dictionary {
        let program = test_truetype(space);
        let mut widths = vec![Object::Integer(250)];
        widths.extend(std::iter::repeat_n(Object::Integer(500), 94));
        dictionary! {
            "Subtype" => "TrueType", "BaseFont" => "TestSans", "Encoding" => "WinAnsiEncoding",
            "FirstChar" => 32, "LastChar" => 126, "Widths" => widths,
            "FontDescriptor" => dictionary! {
                "Type" => "FontDescriptor", "FontName" => "TestSans", "Flags" => 32,
                "FontBBox" => vec![0.into(), (-200).into(), 500.into(), 800.into()],
                "ItalicAngle" => 0, "Ascent" => 800, "Descent" => -200, "CapHeight" => 700,
                "StemV" => 80,
                "FontFile2" => Stream::new(dictionary! { "Length1" => program.len() as i64 }, program),
            },
        }
    }

    #[test]
    #[ignore = "requires local PDFium"]
    fn the_eraser_charges_each_page_content_stream() {
        let block = b"% sample content\n".repeat(32);
        let mut document = Document::load_mem(&fixture(&[&block, &block, &block])).unwrap();
        document.reference_table.cross_reference_type = lopdf::xref::XrefType::CrossReferenceTable;
        document.compress();
        let mut bytes = Vec::new();
        document.save_to(&mut bytes).unwrap();
        // Loading the document fits. Only decoding the targeted page exceeds
        // the individual limit or, separately, the sum across its streams.
        for (single, total) in [(block.len() - 1, 8192), (block.len(), block.len() * 2)] {
            lopdf::StreamBudget::new(single, total)
                .run(|| super::super::parser::with_document(&bytes, |_| Ok::<_, Error>(())))
                .unwrap()
                .unwrap();
            let result = lopdf::StreamBudget::new(single, total)
                .run(|| erase(&bytes, &[target(0.)]).map(|_| ()));
            assert!(matches!(
                result,
                Err(lopdf::Error::Decompress(
                    lopdf::DecompressError::MemoryLimitExceeded { .. }
                ))
            ));
        }
    }

    fn target(x: f64) -> EraseChar {
        EraseChar {
            paragraph: "p1-0".into(),
            page: 0,
            index: 1,
            unicode: 66,
            is_hyphen: false,
            origin: (x, 700.),
            rect: Rect {
                left: x,
                top: 90.,
                right: x + 5.,
                bottom: 100.,
            },
        }
    }
    fn actual_target(bytes: &[u8], x: f64) -> EraseChar {
        let mut c = inplace::characters(bytes, None)
            .unwrap()
            .into_iter()
            .find(|c| c.unicode == 66)
            .unwrap();
        assert!(
            (c.origin.0 - x).abs() < 0.00001,
            "actual {:?}, expected {x}",
            c.origin
        );
        c.paragraph = "p1-0".into();
        c
    }
    fn contents(bytes: &[u8]) -> Vec<Vec<u8>> {
        let doc = Document::load_mem(bytes).unwrap();
        doc.get_page_contents(doc.get_pages()[&1])
            .into_iter()
            .map(|id| {
                doc.get_object(id)
                    .unwrap()
                    .as_stream()
                    .unwrap()
                    .get_plain_content()
                    .unwrap()
            })
            .collect()
    }
    #[test]
    #[ignore = "requires local PDFium"]
    fn tj_and_tj_array_replace_only_the_selected_character() {
        for show in ["(ABC) Tj", "[(A) -100 (BC)] TJ"] {
            let source =
                format!("% before\nq BT /F1 10 Tf 1 0 0 1 100 700 Tm {show} ET Q\n% after\n");
            let x = if show.contains("-100") { 106. } else { 105. };
            let bytes = fixture(&[source.as_bytes()]);
            let (out, report) = erase(&bytes, &[actual_target(&bytes, x)]).unwrap();
            assert_eq!(report.erased, ["p1-0"], "{report:?}");
            let output = contents(&out).remove(0);
            assert!(output.starts_with(b"% before\nq BT /F1 10 Tf 1 0 0 1 100 700 Tm "));
            assert!(output.ends_with(b" ET Q\n% after\n"));
            let ops = Content::decode(&output).unwrap();
            let a = ops
                .operations
                .iter()
                .find(|o| o.operator == "TJ")
                .unwrap()
                .operands[0]
                .as_array()
                .unwrap();
            assert!(a.iter().any(|o| number(o) == Some(-500.)));
            assert!(!a.iter().any(|o| o.as_str().ok() == Some(b"B")));
        }
    }
    #[test]
    #[ignore = "requires local PDFium"]
    fn q_q_text_position_disagreement_retains_the_paragraph() {
        let a = b"q BT /F1 10 Tf 1 0 0 1 100 700 Tm 2 Tc q 4 Tc (A) Tj";
        let b = b" Q (BC) Tj ET Q\n0.12345 0 0 RG 10 20 m 30 40 l S";
        let bytes = fixture(&[a, b]);
        // PDFium restores the text origin here; the independent calculation differs.
        // Such a disagreement must retain the paragraph, not claim a successful edit.
        let (out, r) = erase(&bytes, &[actual_target(&bytes, 100.)]).unwrap();
        assert!(r.erased.is_empty());
        assert!(r.retained.contains_key("p1-0"));
        assert_eq!(out, bytes);
    }
    #[test]
    #[ignore = "requires local PDFium"]
    fn a_text_array_split_across_streams_is_replaced_without_merging_streams() {
        let bytes = fixture(&[b"BT /F1 10 Tf 1 0 0 1 100 700 Tm [(A)", b"(BC)] TJ ET"]);
        let (out, r) = erase(&bytes, &[target(105.)]).unwrap();
        assert_eq!(r.erased, ["p1-0"], "{r:?}");
        let streams = contents(&out);
        assert_eq!(streams.len(), 2);
        assert!(streams[0].ends_with(b"] TJ"));
        let ops = Content::decode(&[streams[0].clone(), streams[1].clone()].concat()).unwrap();
        let array = ops
            .operations
            .iter()
            .find(|o| o.operator == "TJ")
            .unwrap()
            .operands[0]
            .as_array()
            .unwrap();
        assert_eq!(number(&array[1]), Some(-500.));
        assert_eq!(streams[1], b" ET");
    }

    #[test]
    #[ignore = "requires local PDFium"]
    fn line_ending_hyphens_match_and_survive_removing_the_next_line() {
        let bytes = fixture(&[b"BT /F1 10 Tf 100 700 Td (hy-) Tj 0 -12 Td (phen) Tj ET"]);
        let original = inplace::characters(&bytes, None).unwrap();
        assert!(original.iter().any(|c| c.unicode == 2));
        // Both selecting the marked hyphen and leaving it behind must work.
        for next_line_only in [false, true] {
            let mut selected: Vec<_> = original
                .iter()
                .filter(|c| !next_line_only || c.origin.1 < 700.)
                .cloned()
                .collect();
            for c in &mut selected {
                c.paragraph = "p".into();
            }
            let (out, report) = erase(&bytes, &selected).unwrap();
            assert_eq!(report.erased, ["p"], "{report:?}");
            assert_eq!(report.matched_characters, selected.len());
            let (mismatches, _) = inplace::check_erasure(&bytes, &out, &selected, None).unwrap();
            assert!(mismatches.is_empty());
            if next_line_only {
                assert!(
                    inplace::characters(&out, None)
                        .unwrap()
                        .iter()
                        .any(|c| c.unicode == 45)
                );
            }
        }
        let mut a = target(100.);
        a.unicode = 2;
        a.is_hyphen = true;
        let mut b = a.clone();
        b.unicode = 45;
        assert!(inplace::same_character(&a, &b));
        b.origin.0 += 0.00002;
        assert!(!inplace::same_character(&a, &b));
        b.origin = a.origin;
        b.unicode = 0x2010;
        assert!(!inplace::same_character(&a, &b));
    }

    #[test]
    #[ignore = "requires local PDFium"]
    fn an_unreported_second_space_goes_with_the_erased_text_before_it() {
        // Text drawn filled and stroked (`2 Tr`, a synthetic bold), as in real headings:
        // PDFium then reports the second space once the text before it is gone.
        for (show, reported) in [("(AB  ) Tj", 3), ("[(AB ) -20 ( )] TJ", 3)] {
            let source = format!(
                "BT /F1 10 Tf 2 Tr 1 0 0 1 100 700 Tm {show} ET\nBT /F1 10 Tf 1 0 0 1 100 600 Tm (C  D) Tj ET"
            );
            // The space is checked to paint nothing: its font is embedded, its outline empty.
            let bytes = fixture_with(embedded(None), &[source.as_bytes()]);
            let original = inplace::characters(&bytes, None).unwrap();
            // PDFium reports A, B and one space; the second space is not a character.
            let line: Vec<_> = original.iter().filter(|c| c.origin.1 > 650.).collect();
            assert_eq!(line.len(), reported, "{show}: {line:?}");
            let selected: Vec<_> = line
                .iter()
                .map(|c| EraseChar {
                    paragraph: "p".into(),
                    ..(*c).clone()
                })
                .collect();
            let (out, report) = erase(&bytes, &selected).unwrap();
            assert_eq!(report.erased, ["p"], "{show}: {report:?}");
            assert_eq!(report.matched_characters, selected.len());
            let (mismatches, _) = inplace::check_erasure(&bytes, &out, &selected, None).unwrap();
            assert!(mismatches.is_empty());
            // Nothing of the first line is left, and the other line is untouched,
            // including its own unreported space.
            let left = inplace::characters(&out, None).unwrap();
            let before: Vec<_> = original.iter().filter(|c| c.origin.1 < 650.).collect();
            assert_eq!(left.len(), before.len());
            assert!(
                left.iter()
                    .zip(before)
                    .all(|(a, b)| inplace::same_character(a, b))
            );
            let output = contents(&out).remove(0);
            assert!(output.ends_with(b"(C  D) Tj ET"));
        }
        // A space with a character of its own stays a match target as before: erasing
        // only the second line leaves the first line's two spaces alone.
        let bytes = fixture_with(
            embedded(None),
            &[b"BT /F1 10 Tf 2 Tr 1 0 0 1 100 700 Tm (A  ) Tj ET\n\
              BT /F1 10 Tf 1 0 0 1 100 600 Tm (CD) Tj ET"],
        );
        let original = inplace::characters(&bytes, None).unwrap();
        let selected: Vec<_> = original
            .iter()
            .filter(|c| c.origin.1 < 650.)
            .map(|c| EraseChar {
                paragraph: "q".into(),
                ..c.clone()
            })
            .collect();
        let (out, report) = erase(&bytes, &selected).unwrap();
        assert_eq!(report.erased, ["q"], "{report:?}");
        assert!(contents(&out)[0].starts_with(b"BT /F1 10 Tf 2 Tr 1 0 0 1 100 700 Tm (A  ) Tj ET"));
    }

    /// Exercise tightly spaced and widely separated glyphs (TJ adjustments of 1/100 and 12 em):
    /// trailing spaces, isolated spaces, leading spaces, a wide gap within a line, and a
    /// standalone space in a Tj operation. Include both filled and stroked text.
    const SPACED: [(&str, &str); 6] = [
        ("double", "(Gardening group  ) Tj"),
        ("tj", "[(Gardening group ) -10 ( )] TJ"),
        ("far", "[(Gardening group ) -12000 ( )] TJ"),
        ("before", "(  Gardening group) Tj"),
        (
            "boundary",
            "[(Gardening group ) -12000 (  KEEP THIS EQUATION)] TJ",
        ),
        ("separate", "(Gardening group ) Tj ( ) Tj"),
    ];

    /// A line of `show` at 16 pt in [`test_truetype`] with `space`, embedded, and a
    /// paragraph below it.
    fn spaced_line(space: Option<[i16; 4]>, show: &str) -> Vec<u8> {
        let content = format!(
            "BT /F1 16 Tf 0.25 w 2 Tr 1 0 0 1 72 740 Tm {show} ET\n\
             BT /F1 12 Tf 1 0 0 1 72 640 Tm (Separate paragraph remains unchanged.) Tj ET"
        );
        fixture_with(embedded(space), &[content.as_bytes()])
    }

    /// Visible glyphs mapped to a space: a dot below the baseline and a rule.
    const DOT: [i16; 4] = [40, -100, 180, 20];
    const RULE: [i16; 4] = [0, -60, 250, -15];

    #[test]
    #[ignore = "requires local PDFium"]
    fn a_space_that_paints_or_lies_apart_stays_with_its_paint() {
        for space in [Some(DOT), Some(RULE), None] {
            for (kind, show) in SPACED {
                let bytes = spaced_line(space, show);
                let original = inplace::characters(&bytes, None).unwrap();
                let selected: Vec<_> = original
                    .iter()
                    .filter(|c| c.origin.1 > 700.)
                    .map(|c| EraseChar {
                        paragraph: "p".into(),
                        ..c.clone()
                    })
                    .collect();
                let (out, report) = erase(&bytes, &selected).unwrap();
                let erased: Vec<_> = selected
                    .iter()
                    .filter(|c| report.erased.contains(&c.paragraph))
                    .cloned()
                    .collect();
                // Nothing is lost outside the boxes of the characters erased: a painted
                // space next to them or far off is never taken with them.
                let (mismatches, pixels) =
                    inplace::check_erasure(&bytes, &out, &erased, None).unwrap();
                assert!(mismatches.is_empty(), "{space:?} {kind}");
                assert!(
                    pixels.iter().all(|p| p.changed_outside == 0.0),
                    "{space:?} {kind}: {pixels:?}"
                );
                // A space that paints keeps the whole line (its neighbour cannot go without
                // it). One that paints nothing goes with the text next to it, but not with
                // text one `TJ` adjustment of 12 em away, nor from another `Tj`.
                let expected = match (space, kind) {
                    (Some(_), _) => Some(false),
                    (None, "double" | "tj" | "before") => Some(true),
                    (None, "far" | "separate") => Some(false),
                    // Its two spaces lead the line's text far off: they go with it.
                    _ => None,
                };
                if let Some(expected) = expected {
                    assert_eq!(!erased.is_empty(), expected, "{space:?} {kind}: {report:?}");
                }
            }
        }
        // A font that is not embedded cannot tell what its space paints: kept.
        let bytes = fixture(&[b"BT /F1 10 Tf 2 Tr 1 0 0 1 100 700 Tm (AB  ) Tj ET"]);
        let selected: Vec<_> = inplace::characters(&bytes, None)
            .unwrap()
            .into_iter()
            .map(|c| EraseChar {
                paragraph: "p".into(),
                ..c
            })
            .collect();
        let (out, report) = erase(&bytes, &selected).unwrap();
        assert!(report.erased.is_empty(), "{report:?}");
        assert_eq!(out, bytes);
    }

    #[test]
    #[ignore = "requires local PDFium"]
    fn what_a_space_paints_is_drawn_once_for_a_source() {
        let first_line = |bytes: &[u8]| -> Vec<EraseChar> {
            inplace::characters(bytes, None)
                .unwrap()
                .into_iter()
                .filter(|c| c.origin.1 > 700.)
                .map(|c| EraseChar {
                    paragraph: "p".into(),
                    ..c
                })
                .collect()
        };
        let mut blanks = Blanks::default();
        let blank = spaced_line(None, SPACED[0].1);
        let selected = first_line(&blank);
        let (once, report) = erase_with(&blank, &selected, &mut blanks).unwrap();
        assert_eq!(report.erased, ["p"]);
        assert_eq!(blanks.drawn, 1);
        // Erasing the same source again (a replan) draws nothing more, to the same result.
        let (again, report) = erase_with(&blank, &selected, &mut blanks).unwrap();
        assert_eq!((again, report.erased), (once, vec!["p".to_owned()]));
        assert_eq!(blanks.drawn, 1);
        // Another source starts over: its space paints a dot, and the line stays.
        let dotted = spaced_line(Some(DOT), SPACED[0].1);
        let (_, report) = erase_with(&dotted, &first_line(&dotted), &mut blanks).unwrap();
        assert!(report.erased.is_empty(), "{report:?}");
        assert_eq!(blanks.drawn, 1);
    }

    #[test]
    #[ignore = "requires local PDFium and IPA font"]
    fn a_painted_space_is_kept_through_the_file_tab_pipeline() {
        let translations: HashMap<String, String> = (0..4)
            .map(|k| {
                (
                    format!("p1-{k}"),
                    "Translated gardening information.".into(),
                )
            })
            .collect();
        for space in [DOT, RULE] {
            for (kind, show) in SPACED {
                let bytes = spaced_line(Some(space), show);
                let out = super::super::export::write(
                    &bytes,
                    None,
                    &translations,
                    inplace::Options::default(),
                )
                .unwrap();
                // The line is not translated, and nothing of it changes, its spaces' paint
                // included; the paragraph below may be.
                let line = |b: &[u8]| inplace::render_band(b, 0, 1.25, 700.0, 780.0).unwrap();
                assert_eq!(line(&bytes), line(&out.bytes), "{space:?} {kind}");
            }
        }
    }

    /// A ToUnicode CMap without the slash of `/CIDSystemInfo`, as in some InDesign PDFs.
    /// PDFium reads such text; lopdf cannot parse the CMap.
    fn unreadable_cmap(pairs: &[(u8, char)]) -> Stream {
        let mut cmap = String::from(
            "/CIDInit /ProcSet findresource begin\n12 dict begin\nbegincmap\nCIDSystemInfo\n\
             << /Registry (Adobe)\n/Ordering (UCS) /Supplement 0 >> def\n\
             /CMapName /Adobe-Identity-UCS def\n/CMapType 2 def\n1 begincodespacerange\n\
             <0000> <FFFF>\nendcodespacerange\n",
        );
        cmap += &format!("{} beginbfchar\n", pairs.len());
        for (code, ch) in pairs {
            cmap += &format!("<{code:02X}> <{:04X}>\n", u32::from(*ch));
        }
        cmap += "endbfchar\nendcmap CMapName currentdict /CMap defineresource pop end end\n";
        Stream::new(Dictionary::new(), cmap.into_bytes())
    }

    #[test]
    fn an_unreadable_cmap_falls_back_to_the_declared_simple_encoding() {
        let doc = Document::with_version("1.7");
        let font = |extra: Dictionary| {
            let mut d = dictionary! { "Type"=>"Font", "Subtype"=>"Type1", "BaseFont"=>"Helvetica",
            "FirstChar"=>0, "LastChar"=>255, "Widths"=>Object::Array(vec![Object::Integer(500);256]) };
            for (key, value) in extra.iter() {
                d.set(key.clone(), value.clone());
            }
            d
        };
        let broken = unreadable_cmap(&[(0x43, 'C'), (0xE9, 'é')]);
        let unreadable = font(dictionary! {
            "Encoding"=>"WinAnsiEncoding", "ToUnicode"=>Object::Stream(broken.clone())
        });
        // The precondition: lopdf does not read this CMap.
        let mut forced = unreadable.clone();
        forced.set("Encoding", "Identity-H");
        assert!(!matches!(
            forced.get_font_encoding_with_limit(&doc, super::super::parser::STREAM_LIMIT),
            Ok(lopdf::Encoding::UnicodeMapEncoding(_))
        ));
        let read = Font::read(&doc, &unreadable).unwrap();
        assert_eq!(read.unicode[&0xE9], [0xE9]);
        assert_eq!(read.unicode[&0xFE], [0xFE]);
        assert_eq!(read.unicode[&0x43], [0x43]);
        // A readable CMap still decides, even where it differs from the encoding.
        let mut readable = broken.clone();
        readable.set_content(
            String::from_utf8(broken.content.clone())
                .unwrap()
                .replace("\nCIDSystemInfo\n", "\n/CIDSystemInfo\n")
                .replace("<43> <0043>", "<41> <005A>")
                .into_bytes(),
        );
        let mapped = font(dictionary! {
            "Encoding"=>"WinAnsiEncoding", "ToUnicode"=>Object::Stream(readable)
        });
        assert_eq!(Font::read(&doc, &mapped).unwrap().unicode[&0x41], [0x5A]);
        // The fallback applies Differences as for a font without ToUnicode: an
        // unsupported glyph name is never guessed from the code.
        let differences = font(dictionary! {
            "Encoding"=>dictionary! { "BaseEncoding"=>"WinAnsiEncoding",
                "Differences"=>vec![65.into(), Object::Name(b"B".to_vec()), 233.into(),
                    Object::Name(b"uniFFFF".to_vec())] },
            "ToUnicode"=>Object::Stream(broken)
        });
        let read = Font::read(&doc, &differences).unwrap();
        assert_eq!(read.unicode[&65], [u32::from('B')]);
        assert!(!read.unicode.contains_key(&233));
        assert_eq!(read.unicode[&0xFE], [0xFE]);
    }

    #[test]
    #[ignore = "requires local PDFium"]
    fn text_in_a_font_with_an_unreadable_cmap_can_be_erased() {
        let font = dictionary! {
            "Encoding"=>"WinAnsiEncoding",
            "ToUnicode"=>Object::Stream(unreadable_cmap(&[
                (0x43, 'C'), (0x61, 'a'), (0x66, 'f'), (0xE9, 'é'), (0xFE, 'þ'),
            ])),
        };
        let bytes = fixture_with(
            font,
            &[
                b"BT /F1 10 Tf 1 0 0 1 100 700 Tm (Caf\\351) Tj 0 -14 Td (\\376) Tj ET\n\
                BT /F1 10 Tf 1 0 0 1 100 600 Tm (Cafe) Tj ET",
            ],
        );
        let original = inplace::characters(&bytes, None).unwrap();
        assert!(original.iter().any(|c| c.unicode == 0xE9));
        assert!(original.iter().any(|c| c.unicode == 0xFE));
        let selected: Vec<_> = original
            .iter()
            .filter(|c| c.origin.1 > 650.)
            .map(|c| EraseChar {
                paragraph: "p".into(),
                ..c.clone()
            })
            .collect();
        let (out, report) = erase(&bytes, &selected).unwrap();
        assert_eq!(report.erased, ["p"], "{report:?}");
        assert_eq!(report.matched_characters, selected.len());
        let (mismatches, _) = inplace::check_erasure(&bytes, &out, &selected, None).unwrap();
        assert!(mismatches.is_empty());
        // The other line is untouched.
        let left = inplace::characters(&out, None).unwrap();
        assert_eq!(left.len(), 4);
        assert!(left.iter().all(|c| c.origin.1 < 650.));
    }

    #[test]
    fn invalid_output_structure_restores_original_and_preserves_prior_reasons() {
        let bytes = fixture(&[b"BT /F1 10 Tf 100 700 Td (ABC) Tj ET"]);
        let mut doc = Document::load_mem(&bytes).unwrap();
        doc.trailer.set("Size", 1);
        assert!(validate_trailer_size(&doc).is_err());
        let mut broken = bytes.clone();
        let needle = b"/Type/Font";
        let offset = broken
            .windows(needle.len())
            .position(|w| w == needle)
            .unwrap();
        broken[offset + 6] = b']';
        let error = validate_output(&broken).unwrap_err();
        let a = target(100.);
        let mut b = target(105.);
        b.paragraph = "prior".into();
        let mut report = Report {
            erased: vec![a.paragraph.clone()],
            matched_characters: 1,
            ..Report::default()
        };
        report
            .retained
            .insert(b.paragraph.clone(), "prior-reason".into());
        let (out, report) = restore_original(&bytes, &[a, b], report, error);
        assert_eq!(out, bytes);
        assert!(report.erased.is_empty());
        assert_eq!(report.matched_characters, 0);
        assert_eq!(report.retained["prior"], "prior-reason");
        assert_eq!(report.retained["p1-0"], "post-erasure-structure-mismatch");
    }

    #[test]
    fn a_ligature_requires_unicode_agreement_and_one_paragraph() {
        let fonts = HashMap::from([(
            b"F1".to_vec(),
            Font {
                bytes: 1,
                widths: HashMap::from([(65, 500.)]),
                default: None,
                unicode: HashMap::from([(65, vec![102, 105])]),
            },
        )]);
        let mut f = target(100.);
        f.unicode = 102;
        let mut i = target(100.);
        i.unicode = 105;
        i.index = 2;
        let data = b"BT /F1 10 Tf 1 0 0 1 100 700 Tm (A) Tj ET";
        let PlannedEdits {
            matches: counts, ..
        } = edits(
            data,
            &fonts,
            &[&f, &i],
            &HashSet::new(),
            &[&f, &i],
            &[],
            &mut |_, _| false,
        )
        .unwrap();
        assert_eq!(counts, [1, 1]);
        i.paragraph = "another".into();
        let PlannedEdits {
            matches: counts, ..
        } = edits(
            data,
            &fonts,
            &[&f, &i],
            &HashSet::new(),
            &[&f, &i],
            &[],
            &mut |_, _| false,
        )
        .unwrap();
        assert_eq!(counts, [0, 0]);
    }

    #[test]
    #[ignore = "requires local PDFium"]
    fn unmatched_character_keeps_the_entire_paragraph() {
        let bytes = fixture(&[b"BT /F1 10 Tf 1 0 0 1 100 700 Tm (ABC) Tj ET"]);
        let (out, r) = erase(&bytes, &[target(105.), target(999.)]).unwrap();
        assert!(r.erased.is_empty());
        assert_eq!(r.retained.len(), 1);
        assert_eq!(contents(&bytes), contents(&out));
    }

    #[test]
    #[cfg(feature = "pdf-external-tests")]
    #[ignore = "requires local PDFium and qpdf"]
    fn incremental_revision_keeps_original_bytes_and_protected_inputs_are_rejected() {
        let bytes = fixture(&[b"BT /F1 10 Tf 1 0 0 1 100 700 Tm (ABC) Tj ET"]);
        let mut targets = inplace::characters(&bytes, None).unwrap();
        targets.retain(|c| c.unicode == 66);
        targets[0].paragraph = "p".into();
        let (out, report) = erase(&bytes, &targets).unwrap();
        assert_eq!(report.erased, ["p"]);
        assert!(out.starts_with(&bytes));
        let dir = tempfile::tempdir().unwrap();
        let source = dir.path().join("source.pdf");
        let result = dir.path().join("incremental.pdf");
        std::fs::write(&source, &bytes).unwrap();
        std::fs::write(&result, &out).unwrap();
        assert!(
            std::process::Command::new("qpdf")
                .arg("--check")
                .arg(&result)
                .output()
                .unwrap()
                .status
                .success()
        );
        // A second revision exercises xref/object agreement across Prev links.
        assert_eq!(erase(&out, &[]).unwrap().0, out);
        for bits in ["40", "128", "256"] {
            let protected = dir.path().join(format!("protected-{bits}.pdf"));
            assert!(
                std::process::Command::new("qpdf")
                    .args(["--allow-weak-crypto", "--encrypt", "", "owner", bits, "--"])
                    .arg(&source)
                    .arg(&protected)
                    .output()
                    .unwrap()
                    .status
                    .success()
            );
            assert!(matches!(
                erase(&std::fs::read(protected).unwrap(), &targets),
                Err(Error::Protected)
            ));
        }
    }

    #[test]
    #[ignore = "requires local PDFium"]
    fn erasure_checker_rejects_moved_remaining_origins() {
        let a = fixture(&[b"BT /F1 10 Tf 100 700 Td (ABC) Tj ET"]);
        let b = fixture(&[b"BT /F1 10 Tf 100.2 700 Td (ABC) Tj ET"]);
        let (pages, _) = inplace::check_erasure(&a, &b, &[], None).unwrap();
        assert_eq!(pages, [0]);
    }

    #[test]
    #[ignore = "requires local PDFium"]
    fn failed_verification_rolls_back_only_its_page() {
        let bytes = fixture(&[
            b"BT /F1 10 Tf 100 700 Td (ABC) Tj ET BT /F1 10 Tf 100.3 700 Td (ABC) Tj ET",
        ]);
        let mut doc = Document::load_mem(&bytes).unwrap();
        let page_id = doc.get_pages()[&1];
        let mut page = doc.get_object(page_id).unwrap().as_dict().unwrap().clone();
        let parent = page.get(b"Parent").unwrap().as_reference().unwrap();
        let content = doc.add_object(Stream::new(
            Dictionary::new(),
            b"BT /F1 10 Tf 100 700 Td (ABC) Tj ET".to_vec(),
        ));
        page.set("Contents", content);
        let second = doc.add_object(page);
        let pages = doc.get_object_mut(parent).unwrap().as_dict_mut().unwrap();
        pages.set(
            "Kids",
            vec![Object::Reference(second), Object::Reference(page_id)],
        );
        pages.set("Count", 2);
        let mut bytes = Vec::new();
        doc.save_to(&mut bytes).unwrap();
        let mut targets = inplace::characters(&bytes, None).unwrap();
        targets.retain(|c| c.page == 1 || c.unicode == 66);
        for c in &mut targets {
            c.paragraph = format!("page{}", c.page);
        }
        let mut unmatched = targets.iter().find(|c| c.page == 1).unwrap().clone();
        unmatched.paragraph = "prior".into();
        unmatched.origin.0 = 999.;
        targets.push(unmatched);
        let (out, report) = erase(&bytes, &targets).unwrap();
        assert_eq!(report.retained["prior"], "unmatched-or-ambiguous-character");
        assert_eq!(report.matched_characters, 1);
        assert_eq!(report.erased, ["page0"]);
        assert_eq!(
            report.retained["page1"],
            "post-erasure-character-or-origin-mismatch"
        );
        assert!(out.starts_with(&bytes));
        let loaded = Document::load_mem(&out).unwrap();
        assert_eq!(
            loaded.trailer.get(b"Size").unwrap().as_i64().unwrap(),
            i64::from(loaded.reference_table.max_id() + 1)
        );
        let selected: Vec<_> = targets.into_iter().filter(|c| c.page == 0).collect();
        let (mismatches, pixels) = inplace::check_erasure(&bytes, &out, &selected, None).unwrap();
        assert!(mismatches.is_empty());
        assert_eq!(pixels[1].changed_outside, 0.);
    }

    #[test]
    #[ignore = "requires local PDFium"]
    fn a_failed_paragraph_does_not_cancel_a_separate_paragraph_on_its_page() {
        let bytes = fixture(&[b"BT /F1 10 Tf 100 700 Td (ABC) Tj ET BT /F1 10 Tf 100.3 700 Td (ABC) Tj ET BT /F1 10 Tf 100 500 Td (ABC) Tj ET"]);
        let mut targets = inplace::characters(&bytes, None).unwrap();
        for c in &mut targets {
            c.paragraph = if c.origin.1 > 600.0 { "bad" } else { "good" }.into();
        }
        let (_, initial) = erase_once(&bytes, &targets).unwrap();
        assert!(!initial.erased.contains(&"good".into()));
        let (out, report) = erase(&bytes, &targets).unwrap();
        assert_eq!(report.erased, ["good"]);
        assert!(report.retained.contains_key("bad"));
        let selected: Vec<_> = targets
            .into_iter()
            .filter(|c| c.paragraph == "good")
            .collect();
        let (mismatch, pixels) = inplace::check_erasure(&bytes, &out, &selected, None).unwrap();
        assert!(mismatch.is_empty());
        assert!(pixels.iter().all(|p| p.changed_outside == 0.0));
    }

    #[test]
    fn unicode_and_unselected_overlaps_are_required_for_a_match() {
        let fonts = HashMap::from([(
            b"F1".to_vec(),
            Font {
                bytes: 1,
                widths: HashMap::from([(65, 500.)]),
                default: None,
                unicode: HashMap::from([(65, vec![65])]),
            },
        )]);
        let data = b"BT /F1 10 Tf 1 0 0 1 100 700 Tm (A) Tj ET";
        let mut a = target(100.);
        let PlannedEdits {
            matches: counts, ..
        } = edits(
            data,
            &fonts,
            &[&a],
            &HashSet::new(),
            &[&a],
            &[],
            &mut |_, _| false,
        )
        .unwrap();
        assert_eq!(counts, [0]); // B at A's origin is not a match.
        a.unicode = 65;
        let mut unselected = a.clone();
        unselected.index += 1;
        let PlannedEdits {
            matches: counts, ..
        } = edits(
            data,
            &fonts,
            &[&a],
            &HashSet::new(),
            &[&a, &unselected],
            &[],
            &mut |_, _| false,
        )
        .unwrap();
        assert_eq!(counts, [0]);
        assert_eq!(glyph_unicode(b"hyphen"), Some(vec![45]));
        assert_eq!(glyph_unicode(b"unknown"), None);
    }

    #[test]
    fn lexer_ignores_operators_inside_strings_comments_and_containers() {
        let data = b"% q Tj\nBT /F1 10 Tf [(a\\(TJ\\)b) 12 <5142>] TJ ET\n";
        let ops = spans(data, &[]).unwrap();
        assert_eq!(
            ops.iter()
                .map(|(_, o, _)| o.operator.as_str())
                .collect::<Vec<_>>(),
            ["BT", "Tf", "TJ", "ET"]
        );
        assert!(spans(b"BI /W 1 ID xx EI", &[]).is_err());
    }
}
