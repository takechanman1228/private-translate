//! Native Word does the final pagination. Only grow floating table boxes when
//! the document already wraps surrounding text around their boundary.
use super::{
    DRAWING, Error, OfficeTarget, Paragraph, STRICT_DRAWING, STRICT_WORD, TagFormat, WORD,
};
use quick_xml::{events::Event, name::ResolveResult, reader::NsReader};
use std::ops::Range;

type Edit = (Range<usize>, String);
const MC: &[u8] = b"http://schemas.openxmlformats.org/markup-compatibility/2006";

#[derive(Default)]
pub(super) struct Region {
    range: Range<usize>,
    table: bool,
    textbox: bool,
    wraps: bool,
    behind: bool,
    fixed_row: bool,
    autofit: Vec<Edit>,
    vml_fit: Option<Edit>,
}

pub(super) struct Decision {
    pub paragraph: usize,
    pub edits: Vec<Edit>,
}

impl Region {
    pub(super) fn decide(
        &self,
        paragraphs: &[Paragraph],
        targets: &[Option<OfficeTarget>],
        format: TagFormat,
    ) -> Option<Decision> {
        let start = paragraphs.partition_point(|p| p.body.start < self.range.start);
        let end = paragraphs.partition_point(|p| p.body.start < self.range.end);
        for (p, t) in paragraphs[start..end]
            .iter()
            .zip(targets.iter().skip(start))
        {
            let Some(text) = t
                .as_ref()
                .and_then(|t| p.visible_office_target(format, t).ok())
            else {
                continue;
            };
            let source = p.plain_text();
            if text == source
                || (!self.table
                    && super::layout::text_width(&text) <= super::layout::text_width(&source)
                    && text.lines().count() <= source.lines().count())
            {
                continue;
            }
            return Some(Decision {
                paragraph: p.word_location()?.0,
                edits: if self.table && self.textbox && self.wraps && !self.behind {
                    self.autofit.clone()
                } else {
                    Vec::new()
                },
            });
        }
        None
    }
}

/// Pair only a single Choice shape and a single fallback table textbox in the
/// same AlternateContent. Ambiguous branches keep their native dimensions.
struct Alternate {
    depth: usize,
    first: usize,
    fallback: Option<usize>,
    choices: usize,
    textboxes: usize,
    ambiguous: bool,
}

impl Alternate {
    fn finish(self, regions: &mut [Region]) {
        let members = &mut regions[self.first..];
        if !self.ambiguous
            && self.choices == 1
            && self.textboxes == 1
            && let Some(split) = self.fallback
        {
            let (choice, fallback) = members.split_at_mut(split - self.first);
            let mut shapes = choice.iter_mut().filter(|r| r.textbox);
            let mut copies = fallback.iter().filter(|r| r.textbox);
            if let (Some(shape), Some(copy)) = (shapes.next(), copies.next())
                && shapes.next().is_none()
                && copies.next().is_none()
                && shape.autofit.len() == 1
                && copy.table
                && let Some(edit) = &copy.vml_fit
            {
                shape.autofit.push(edit.clone());
                return;
            }
        }
        for region in members {
            region.autofit.clear();
        }
    }
}

fn fit_vml_textbox(
    tag: &quick_xml::events::BytesStart<'_>,
    reader: &NsReader<&[u8]>,
) -> Result<String, Error> {
    use quick_xml::{XmlVersion, events::BytesStart};
    let qname = tag.name();
    let mut result = BytesStart::new(std::str::from_utf8(qname.as_ref()).map_err(|_| Error::Xml)?);
    let mut style = String::new();
    for a in tag.attributes() {
        let a = a.map_err(|_| Error::Xml)?;
        let name = std::str::from_utf8(a.key.as_ref()).map_err(|_| Error::Xml)?;
        let value = a
            .decoded_and_normalized_value(XmlVersion::Implicit1_0, reader.decoder())
            .map_err(|_| Error::Xml)?;
        if name == "style" {
            for declaration in value.split(';').filter(|d| !d.trim().is_empty()) {
                if !declaration
                    .split_once(':')
                    .is_some_and(|(k, _)| k.trim().eq_ignore_ascii_case("mso-fit-shape-to-text"))
                {
                    style.push_str(declaration);
                    style.push(';');
                }
            }
        } else {
            result.push_attribute((name, value.as_ref()));
        }
    }
    style.push_str("mso-fit-shape-to-text:t");
    result.push_attribute(("style", style.as_str()));
    Ok(format!(
        "<{}>",
        std::str::from_utf8(&result).map_err(|_| Error::Xml)?
    ))
}

pub(super) fn regions(xml: &str) -> Result<Vec<Region>, Error> {
    let mut reader = NsReader::from_str(xml);
    let mut stack = Vec::<Vec<u8>>::new();
    let mut active: Vec<(usize, Region)> = Vec::new();
    let mut regions = Vec::new();
    let mut alternatives = Vec::<Alternate>::new();
    loop {
        let start = reader.buffer_position() as usize;
        let (ns, event) = reader.read_resolved_event().map_err(|_| Error::Xml)?;
        let uri = match &ns {
            ResolveResult::Bound(n) => n.as_ref(),
            _ => b"",
        };
        match &event {
            Event::Start(tag) | Event::Empty(tag) => {
                let name = tag.local_name();
                let name = name.as_ref();
                let drawingml = uri == DRAWING || uri == STRICT_DRAWING;
                let word = uri == WORD || uri == STRICT_WORD;
                let drawing = uri.ends_with(b"/wordprocessingDrawing");
                let vml = uri == b"urn:schemas-microsoft-com:vml";
                let attr = |k: &[u8], v: &[u8]| {
                    tag.attributes()
                        .flatten()
                        .any(|a| a.key.local_name().as_ref() == k && a.value.as_ref() == v)
                };
                let open = matches!(event, Event::Start(_));
                if uri == MC && name == b"AlternateContent" && open {
                    let nested = !alternatives.is_empty();
                    if let Some(parent) = alternatives.last_mut() {
                        parent.ambiguous = true;
                    }
                    alternatives.push(Alternate {
                        depth: stack.len(),
                        first: regions.len(),
                        fallback: None,
                        choices: 0,
                        textboxes: 0,
                        ambiguous: nested,
                    });
                } else if let Some(group) = alternatives.last_mut() {
                    if stack.len() == group.depth + 1 {
                        match (uri, name) {
                            (MC, b"Choice") if group.fallback.is_none() => group.choices += 1,
                            (MC, b"Fallback") if group.fallback.is_none() => {
                                group.fallback = Some(regions.len())
                            }
                            _ => group.ambiguous = true,
                        }
                    }
                    if vml && name == b"textbox" {
                        group.textboxes += 1;
                    }
                }
                if open
                    && (drawing && matches!(name, b"anchor" | b"inline")
                        || word && name == b"tr"
                        || vml && name == b"shape")
                {
                    active.push((
                        stack.len(),
                        Region {
                            range: start..start,
                            behind: attr(b"behindDoc", b"1") || attr(b"behindDoc", b"true"),
                            ..Default::default()
                        },
                    ));
                }
                if word && name == b"tbl" {
                    for (_, r) in &mut active {
                        if r.textbox {
                            r.table = true;
                        }
                    }
                }
                if let Some((_, r)) = active.last_mut() {
                    if open
                        && vml
                        && name == b"textbox"
                        && stack.last().is_some_and(|p| p.as_slice() == b"shape")
                        && !alternatives.is_empty()
                    {
                        r.vml_fit = Some((
                            start..reader.buffer_position() as usize,
                            fit_vml_textbox(tag, &reader)?,
                        ));
                    }
                    if word && name == b"txbxContent" {
                        r.textbox = true;
                    }
                    if drawing
                        && matches!(name, b"wrapSquare" | b"wrapTopAndBottom")
                        && stack.last().is_some_and(|n: &Vec<u8>| n == b"anchor")
                    {
                        r.wraps = true;
                    }
                    if word && name == b"trHeight" && attr(b"hRule", b"exact") {
                        r.fixed_row = true;
                    }
                    if drawingml
                        && name == b"noAutofit"
                        && !open
                        && stack.ends_with(&[b"wsp".to_vec(), b"bodyPr".to_vec()])
                    {
                        let end = reader.buffer_position() as usize;
                        r.autofit.push((
                            start..end,
                            xml[start..end].replacen("noAutofit", "spAutoFit", 1),
                        ));
                    }
                }
                if open {
                    stack.push(name.to_vec());
                }
            }
            Event::End(_) => {
                stack.pop();
                if let Some(alternate) = alternatives.pop_if(|a| a.depth == stack.len()) {
                    alternate.finish(&mut regions);
                }
                if active
                    .last()
                    .is_some_and(|(depth, _)| *depth == stack.len())
                {
                    let (_, mut r) = active.pop().unwrap();
                    if r.textbox || r.fixed_row {
                        r.range.end = start;
                        regions.push(r);
                    }
                }
            }
            Event::Eof => return Ok(regions),
            _ => {}
        }
    }
}
