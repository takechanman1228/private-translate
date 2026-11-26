//! Keep the missing East Asian symbol face local to its text body. Impress
//! retains a run's Asian face on its insertion cursor when the following run
//! supplies no face. Automatic numbers use the script of the first text.
use super::Error;
use quick_xml::{Reader, events::Event};
use std::ops::Range;

#[derive(Default)]
struct Run {
    start: usize,
    end: usize,
    properties: Option<Range<usize>>,
    east_asian: Option<String>,
    symbol: Option<String>,
    visible: bool,
    private: bool,
}

fn face(tag: &quick_xml::events::BytesStart<'_>) -> Option<String> {
    tag.attributes()
        .flatten()
        .find(|a| a.key.as_ref() == b"typeface")
        .and_then(|a| String::from_utf8(a.value.into_owned()).ok())
}

fn prefix(tag: &str) -> &str {
    let name = tag
        .trim_start_matches('<')
        .split([' ', '\t', '\n', '\r', '/', '>'])
        .next()
        .unwrap_or("");
    name.rfind(':').map_or("", |i| &name[..=i])
}

/// Apply edits in source coordinates, then scope fonts in the resulting run
/// order. Rendering and SmartArt cache copies share the same translation mask.
pub(super) fn apply<'a>(
    source: &str,
    mut edits: Vec<(Range<usize>, String)>,
    paragraphs: impl Iterator<Item = (&'a super::Paragraph, bool)>,
    theme: &std::collections::HashMap<String, String>,
    drawing: bool,
) -> Result<String, Error> {
    edits.sort_unstable_by_key(|(r, _)| r.start);
    let mut translated = Vec::new();
    let mut defaults = Vec::new();
    if drawing && source.contains("sym") {
        let mut offset = 0isize;
        let mut changes = edits.iter().peekable();
        for (p, is_translated) in paragraphs {
            while let Some((r, value)) = changes.next_if(|(r, _)| r.end <= p.body.start) {
                offset += value.len() as isize - r.len() as isize;
            }
            let start = p.body.start.wrapping_add_signed(offset);
            if let Some(f) = &p.inherited_ea {
                defaults.push((start, f.clone()));
            }
            if is_translated
                && let Some((r, value)) = changes.peek()
                && *r == p.body
            {
                translated.push(start..start + value.len());
            }
        }
    }
    let mut xml = source.to_owned();
    for (r, value) in edits.into_iter().rev() {
        xml.replace_range(r, &value);
    }
    if !translated.is_empty() {
        scope(&xml, &translated, &defaults, theme)
    } else {
        Ok(xml)
    }
}

/// Inspect the final run order, including untranslated text, before choosing
/// the place to restore a missing font. Offsets refer to the same input XML.
fn scope(
    xml: &str,
    translated: &[Range<usize>],
    defaults: &[(usize, String)],
    theme: &std::collections::HashMap<String, String>,
) -> Result<String, Error> {
    if !xml.contains("sym") {
        return Ok(xml.into());
    }
    let mut reader = quick_xml::NsReader::from_str(xml);
    let mut stack = Vec::new();
    let mut run = None;
    let mut previous = None::<String>;
    let mut restore = false;
    let mut edits = Vec::new();
    let mut inherited = None;
    let mut parents = Vec::new();
    let mut body = None;
    loop {
        let start = reader.buffer_position() as usize;
        let (ns, event) = reader.read_resolved_event().map_err(|_| Error::Xml)?;
        let drawing = matches!(ns, quick_xml::name::ResolveResult::Bound(ns) if [super::DRAWING,super::STRICT_DRAWING].contains(&ns.as_ref()));
        let end = reader.buffer_position() as usize;
        match &event {
            Event::Start(tag) | Event::Empty(tag) => {
                let q = tag.local_name();
                let name = q.as_ref();
                let parent = stack.last().map(Vec::as_slice);
                if drawing {
                    match name {
                        b"p" => {
                            // A chart's c:rich, a diagram's dgm:t and a shape's
                            // txBody all own independent DrawingML paragraphs.
                            if body != parents.last().copied() {
                                previous = None;
                                restore = false;
                                body = parents.last().copied();
                            }
                            inherited = defaults
                                .binary_search_by_key(&end, |(offset, _)| *offset)
                                .ok()
                                .map(|i| defaults[i].1.clone());
                        }
                        b"r" | b"fld" if parent == Some(b"p") => {
                            run = Some(Run {
                                start,
                                end,
                                ..Default::default()
                            });
                        }
                        b"rPr" if let Some(r) = &mut run => r.properties = Some(start..end),
                        b"ea"
                            if parent == Some(b"defRPr")
                                && stack.iter().rev().nth(2).is_some_and(|n| n == b"pPr") =>
                        {
                            if let Some(f) = face(tag).filter(|f| !f.is_empty()) {
                                inherited = Some(f);
                            }
                        }
                        b"ea"
                            if parent == Some(b"rPr")
                                && let Some(r) = &mut run =>
                        {
                            r.east_asian = face(tag)
                        }
                        b"sym"
                            if parent == Some(b"rPr")
                                && let Some(r) = &mut run =>
                        {
                            r.symbol = face(tag).filter(|f| !f.is_empty());
                        }
                        _ => {}
                    }
                }
                if matches!(event, Event::Start(_)) {
                    stack.push(name.to_vec());
                    parents.push(start);
                }
            }
            Event::Text(t)
                if stack.last().is_some_and(|n| n == b"t")
                    && let Some(r) = &mut run =>
            {
                let t = t.decode().map_err(|_| Error::Xml)?;
                r.private = (!r.visible || r.private)
                    && t.chars().all(|c| ('\u{e000}'..='\u{f8ff}').contains(&c));
                r.visible |= !t.is_empty();
            }
            Event::GeneralRef(_) | Event::CData(_)
                if stack.last().is_some_and(|n| n == b"t")
                    && let Some(r) = &mut run =>
            {
                // Rebuilt private characters are literal UTF-8. A retained
                // entity or CDATA section still consumes the pending restore.
                r.visible = true;
                r.private = false;
            }
            Event::End(tag) => {
                if drawing {
                    match tag.local_name().as_ref() {
                        b"r" | b"fld"
                            if let Some(r) = run.take()
                                && r.visible =>
                        {
                            let resolved = |f: &String| theme.get(f).unwrap_or(f).clone();
                            let normal = r
                                .east_asian
                                .as_ref()
                                .or(inherited.as_ref())
                                .map(resolved)
                                .filter(|f| !f.is_empty());
                            if let Some(f) = &normal {
                                previous = Some(f.clone());
                            }
                            let i = translated.partition_point(|v| v.end <= r.start);
                            let font = if r.private
                                && r.east_asian.is_none()
                                && let Some(f) = &r.symbol
                                && translated
                                    .get(i)
                                    .is_some_and(|span| span.contains(&r.start))
                            {
                                restore = true;
                                Some(f.as_str())
                            } else if std::mem::take(&mut restore) && normal.is_none() {
                                // The standard Impress style supplies this face when
                                // the package and preceding runs leave Asian text unstyled.
                                Some(previous.as_deref().unwrap_or("Arial Unicode MS"))
                            } else {
                                None
                            };
                            if let Some(font) = font {
                                let p = prefix(&xml[r.start..r.end]);
                                let font = font.replace('"', "&quot;");
                                let ea = format!("<{p}ea typeface=\"{font}\"/>");
                                if let Some(range) = r.properties {
                                    edits
                                        .push((range.clone(), insert_ea(&xml[range], &ea, &font)?));
                                } else {
                                    edits.push((r.end..r.end, format!("<{p}rPr>{ea}</{p}rPr>")));
                                }
                            }
                        }
                        b"rPr"
                            if let Some(range) =
                                run.as_mut().and_then(|r| r.properties.as_mut()) =>
                        {
                            range.end = end
                        }
                        _ => {}
                    }
                }
                stack.pop();
                parents.pop();
            }
            Event::Eof => break,
            _ => {}
        }
    }
    let mut out = xml.to_owned();
    for (r, value) in edits.into_iter().rev() {
        out.replace_range(r, &value);
    }
    Ok(out)
}

fn insert_ea(xml: &str, ea: &str, font: &str) -> Result<String, Error> {
    let mut reader = Reader::from_str(xml);
    let first = reader.read_event().map_err(|_| Error::Xml)?;
    if let Event::Empty(t) = first {
        let q = t.name();
        let name = std::str::from_utf8(q.as_ref()).map_err(|_| Error::Xml)?;
        return Ok(format!("{}>{ea}</{name}>", xml.trim_end_matches("/>")));
    }
    let mut depth = 0;
    loop {
        let start = reader.buffer_position() as usize;
        let e = reader.read_event().map_err(|_| Error::Xml)?;
        match e {
            Event::Start(ref t) | Event::Empty(ref t)
                if depth == 0
                    && matches!(
                        t.local_name().as_ref(),
                        b"ea"
                            | b"cs"
                            | b"sym"
                            | b"hlinkClick"
                            | b"hlinkMouseOver"
                            | b"rtl"
                            | b"extLst"
                    ) =>
            {
                if t.local_name().as_ref() == b"ea" {
                    let end = reader.buffer_position() as usize;
                    let tag = super::layout::rewrite_tag(&xml[start..end], &[(b"typeface", font)])?;
                    return Ok(format!("{}{tag}{}", &xml[..start], &xml[end..]));
                }
                return Ok(format!("{}{ea}{}", &xml[..start], &xml[start..]));
            }
            Event::Start(_) => depth += 1,
            Event::End(_) if depth == 0 => {
                return Ok(format!("{}{ea}{}", &xml[..start], &xml[start..]));
            }
            Event::End(_) => depth -= 1,
            Event::Eof => return Err(Error::Xml),
            _ => {}
        }
    }
}

pub(super) type Faces = [Option<String>; 9];

/// DrawingML list defaults can apply to every level or to one level.
pub(super) fn faces(xml: &str, container: &[u8]) -> Result<Faces, Error> {
    let mut result: Faces = Default::default();
    let mut default = None;
    let mut stack = Vec::<Vec<u8>>::new();
    let mut reader = Reader::from_str(xml);
    loop {
        let event = reader.read_event().map_err(|_| Error::Xml)?;
        match &event {
            Event::Start(t) | Event::Empty(t) => {
                let name = t.local_name();
                if name.as_ref() == b"ea"
                    && let [.., c, level, pr] = stack.as_slice()
                    && c == container
                    && pr == b"defRPr"
                {
                    if level == b"defPPr" {
                        default = face(t);
                    } else if let [b'l', b'v', b'l', digit @ b'1'..=b'9', b'p', b'P', b'r'] =
                        level.as_slice()
                    {
                        result[(digit - b'1') as usize] = face(t);
                    }
                }
                if matches!(event, Event::Start(_)) {
                    stack.push(name.as_ref().to_vec());
                }
            }
            Event::End(_) => {
                stack.pop();
            }
            Event::Eof => break,
            _ => {}
        }
    }
    Ok(result.map(|f| f.or_else(|| default.clone())))
}

pub(super) fn theme_fonts(xml: &str) -> Result<std::collections::HashMap<String, String>, Error> {
    let mut reader = Reader::from_str(xml);
    let mut prefix = None;
    let mut names = std::collections::HashMap::new();
    loop {
        match reader.read_event().map_err(|_| Error::Xml)? {
            Event::Start(t) if t.local_name().as_ref() == b"majorFont" => prefix = Some("+mj-"),
            Event::Start(t) if t.local_name().as_ref() == b"minorFont" => prefix = Some("+mn-"),
            Event::End(t) if matches!(t.local_name().as_ref(), b"majorFont" | b"minorFont") => {
                prefix = None
            }
            Event::Start(t) | Event::Empty(t) if prefix.is_some() => {
                let slot = match t.local_name().as_ref() {
                    b"latin" => "lt",
                    b"ea" => "ea",
                    b"cs" => "cs",
                    _ => continue,
                };
                if let Some(f) = face(&t) {
                    names.insert(format!("{}{slot}", prefix.unwrap()), f);
                }
            }
            Event::Eof => return Ok(names),
            _ => {}
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn restores_the_preceding_face_and_respects_the_next_runs_theme() {
        let symbol = "<a:r><a:rPr><a:cs typeface='Other'/><a:sym typeface='Wingdings'/></a:rPr><a:t>\u{f04a}</a:t></a:r>";
        let before = "<a:r><a:rPr><a:ea typeface='Book'/></a:rPr><a:t>案内</a:t></a:r>";
        let next = "<a:r><a:rPr/><a:t>次</a:t></a:r>";
        let theme = [("+mn-ea".into(), "Other book".into())].into();
        for (source, expected) in [
            (next, "Book"),
            (
                "<a:r><a:rPr><a:ea typeface='+mn-ea'/></a:rPr><a:t>次</a:t></a:r>",
                "+mn-ea",
            ),
        ] {
            let xml = format!(
                "<p:txBody xmlns:p='urn:p' xmlns:a='http://schemas.openxmlformats.org/drawingml/2006/main'><a:p>{before}{symbol}</a:p><a:p><a:pPr><a:buAutoNum type='arabicParenR'/></a:pPr>{source}</a:p></p:txBody>"
            );
            let start = xml.find(symbol).unwrap();
            let range = start..start + symbol.len();
            let out = scope(&xml, std::slice::from_ref(&range), &[], &theme).unwrap();
            assert!(out.contains("<a:ea typeface=\"Wingdings\"/><a:cs"));
            let following = out.split("buAutoNum").nth(1).unwrap();
            assert!(following.contains(expected));
            assert!(!following.contains("Wingdings"));
            assert_eq!(scope(&xml, &[], &[], &theme).unwrap(), xml);
        }
    }
}
