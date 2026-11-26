use std::io::{Read, Seek};
use std::ops::Range;

use quick_xml::{
    XmlVersion,
    events::{BytesStart, Event},
    name::ResolveResult,
    reader::NsReader,
};
use zip::ZipArchive;

use super::{Error, Paragraph, TagFormat, inheritance};

const CHART: &[&[u8]] = &[
    b"http://schemas.openxmlformats.org/drawingml/2006/chart",
    b"http://purl.oclc.org/ooxml/drawingml/chart",
];
const DIAGRAM: &[&[u8]] = &[
    b"http://schemas.openxmlformats.org/drawingml/2006/diagram",
    b"http://purl.oclc.org/ooxml/drawingml/diagram",
];
const DIAGRAM_EXT: &[u8] = b"http://schemas.microsoft.com/office/drawing/2008/diagram";

#[derive(Clone, Copy)]
pub(super) enum Graphic {
    Chart,
    Diagram,
}

impl Graphic {
    pub(super) fn of(name: &str) -> Option<Self> {
        [
            ("ppt/charts/chart", Self::Chart),
            ("ppt/diagrams/data", Self::Diagram),
        ]
        .into_iter()
        .find_map(|(prefix, kind)| {
            name.strip_prefix(prefix)?
                .strip_suffix(".xml")
                .filter(|n| !n.is_empty() && n.bytes().all(|b| b.is_ascii_digit()))
                .map(|_| kind)
        })
    }

    // Body ranges accepted by the ordinary DrawingML paragraph parser. Using
    // the complete part keeps prefix bindings and original byte offsets intact.
    pub(super) fn scopes(self, xml: &str) -> Result<(Vec<Range<usize>>, Option<String>), Error> {
        #[derive(Clone, Copy, PartialEq, Eq)]
        enum Element {
            Title,
            ChartText,
            Rich,
            Point,
            DiagramText,
            Other,
        }
        let mut reader = NsReader::from_str(xml);
        let mut stack = Vec::new();
        let mut ranges = Vec::new();
        let mut cache = None;
        loop {
            let at = reader.buffer_position() as usize;
            let (ns, event) = reader.read_resolved_event().map_err(|_| Error::Xml)?;
            let opened = matches!(&event, Event::Start(_));
            match event {
                Event::Start(tag) | Event::Empty(tag) => {
                    let name = tag.local_name();
                    let ns = match ns {
                        ResolveResult::Bound(ns) => ns.into_inner(),
                        _ => b"",
                    };
                    let element = match (self, name.as_ref()) {
                        (Self::Chart, b"title") if CHART.contains(&ns) => Element::Title,
                        (Self::Chart, b"tx") if CHART.contains(&ns) => Element::ChartText,
                        (Self::Chart, b"rich") if CHART.contains(&ns) => Element::Rich,
                        (Self::Diagram, b"pt") if DIAGRAM.contains(&ns) => Element::Point,
                        (Self::Diagram, b"t") if DIAGRAM.contains(&ns) => Element::DiagramText,
                        (Self::Diagram, b"dataModelExt") if ns == DIAGRAM_EXT => {
                            if let Some(id) = attr(&tag, b"relId")?
                                && cache.replace(id).is_some()
                            {
                                return Err(Error::Xml);
                            }
                            Element::Other
                        }
                        _ => Element::Other,
                    };
                    if opened {
                        stack.push((element, reader.buffer_position() as usize));
                    }
                }
                Event::End(_) => {
                    let (element, start) = stack.pop().ok_or(Error::Xml)?;
                    let in_scope = match element {
                        Element::Rich => matches!(
                            stack.as_slice(),
                            [.., (Element::Title, _), (Element::ChartText, _)]
                        ),
                        Element::DiagramText => matches!(stack.last(), Some((Element::Point, _))),
                        _ => false,
                    };
                    if in_scope {
                        ranges.push(start..at);
                    }
                }
                Event::DocType(_) => return Err(Error::Xml),
                Event::Eof if stack.is_empty() => return Ok((ranges, cache)),
                Event::Eof => return Err(Error::Xml),
                _ => {}
            }
        }
    }
}

fn attr(tag: &BytesStart<'_>, key: &[u8]) -> Result<Option<String>, Error> {
    let attr = tag.try_get_attribute(key).map_err(|_| Error::Xml)?;
    attr.map(|attr| {
        attr.decoded_and_normalized_value(
            XmlVersion::Implicit1_0,
            quick_xml::Reader::from_str("").decoder(),
        )
        .map(|value| value.into_owned())
        .map_err(|_| Error::Xml)
    })
    .transpose()
}

pub(super) struct Cache {
    pub(super) index: usize,
    pub(super) name: String,
    pub(super) xml: String,
    pub(super) links: Vec<(usize, Paragraph)>,
}

pub(super) fn diagram_cache<R: Read + Seek>(
    archive: &mut ZipArchive<R>,
    data: &str,
    id: Option<&str>,
    paragraphs: &[Paragraph],
) -> Result<Option<Option<Cache>>, Error> {
    let Some(id) = id else {
        return Ok(Some(None));
    };
    let cache = inheritance::related_id(archive, data, "diagramDrawing", Some(id))?;
    let cache = if cache.is_some() {
        cache
    } else {
        // Earlier Office files refer to the drawing via the slide's relationship
        // part. Require both references to resolve from the same slide.
        let slides: Vec<_> = archive
            .file_names()
            .filter_map(Result::ok)
            .filter(|n| super::slide_or_notes(n) && n.starts_with("ppt/slides/"))
            .map(std::borrow::Cow::into_owned)
            .collect();
        let mut found = None;
        for slide in slides {
            if !inheritance::related_targets(archive, &slide, "diagramData")?
                .iter()
                .any(|(_, target)| target == data)
            {
                continue;
            }
            if let Some(candidate) =
                inheritance::related_id(archive, &slide, "diagramDrawing", Some(id))?
            {
                if found.as_ref().is_some_and(|path| *path != candidate) {
                    return Ok(None);
                }
                found = Some(candidate);
            }
        }
        found
    };
    let Some(path) =
        cache.filter(|path| path.starts_with("ppt/diagrams/drawing") && path.ends_with(".xml"))
    else {
        return Ok(None);
    };
    let Some(index) = archive.index_for_name(&path) else {
        return Ok(None);
    };
    let Some(xml) = inheritance::xml(archive, &path)? else {
        return Ok(None);
    };
    let (cached, _) = super::parse_paragraph_details(&xml, false, super::Format::PowerPoint)?;
    let mut sources = std::collections::HashMap::new();
    for (index, paragraph) in paragraphs.iter().enumerate() {
        sources
            .entry(paragraph.source(TagFormat::Anchor))
            .and_modify(|slot| *slot = None)
            .or_insert(Some(index));
    }
    let mut links = Vec::new();
    let mut matched = std::collections::HashSet::new();
    for paragraph in cached {
        let source = paragraph.source(TagFormat::Anchor);
        let Some(Some(index)) = sources.get(&source) else {
            return Ok(None);
        };
        if !matched.insert(*index) {
            return Ok(None);
        }
        links.push((*index, paragraph));
    }
    // Empty caches are regenerated by PowerPoint. A populated cache must
    // cover every supported paragraph before we update either representation.
    if links.is_empty() {
        return Ok((!super::has_untranslated_text(&xml)?).then_some(None));
    }
    if links.len() != paragraphs.len()
        || super::has_text_outside(&xml, links.iter().map(|(_, p)| &p.body))?
    {
        return Ok(None);
    }
    Ok(Some(Some(Cache {
        index,
        name: path,
        xml,
        links,
    })))
}

#[cfg(test)]
mod tests {
    use crate::office::{OfficeDocument, TagFormat};
    use std::io::{Cursor, Read, Write};
    use zip::{ZipArchive, ZipWriter, write::SimpleFileOptions};

    #[test]
    fn chart_titles_and_smartart_caches_round_trip_without_touching_other_parts() {
        const DRAWING: &str = "http://schemas.openxmlformats.org/drawingml/2006/main";
        let paragraph = |text| format!("<a:p><a:r><a:t>{text}</a:t></a:r></a:p>");
        let title = |text| {
            format!(
                "<c:title><c:tx><c:rich>{}</c:rich></c:tx></c:title>",
                paragraph(text)
            )
        };
        let chart = format!(
            r#"<c:chartSpace xmlns:a="{DRAWING}" xmlns:c="http://schemas.openxmlformats.org/drawingml/2006/chart"><c:chart>{}<c:valAx>{}</c:valAx><c:ser><c:strCache><c:pt><c:v>Category label</c:v></c:pt></c:strCache><c:numCache><c:pt><c:v>12.5</c:v></c:pt></c:numCache></c:ser><c:dLbl><c:tx><c:rich>{}</c:rich></c:tx></c:dLbl></c:chart></c:chartSpace>"#,
            title("Revenue by year"),
            title("Annual revenue"),
            paragraph("Untouched data label")
        );
        let diagram = format!(
            r#"<d:dataModel xmlns:a="{DRAWING}" xmlns:d="http://schemas.openxmlformats.org/drawingml/2006/diagram" xmlns:e="http://schemas.microsoft.com/office/drawing/2008/diagram"><d:ptLst><d:pt><d:t>{}</d:t></d:pt></d:ptLst><e:dataModelExt relId="cache"/></d:dataModel>"#,
            paragraph("Plan ahead")
        );
        let drawing = format!(
            r#"<d:drawing xmlns:a="{DRAWING}" xmlns:d="http://schemas.microsoft.com/office/drawing/2008/diagram"><d:sp><a:xfrm/><d:txBody>{}</d:txBody></d:sp></d:drawing>"#,
            paragraph("Plan ahead")
        );
        for compatible in [true, false] {
            let drawing = if compatible {
                drawing.clone()
            } else {
                drawing.replace("Plan ahead", "Different label")
            };
            let rels = r#"<Relationships><Relationship Id="cache" Type="x/diagramDrawing" Target="drawing3.xml"/></Relationships>"#;
            let mut zip = ZipWriter::new(Cursor::new(Vec::new()));
            for (name, contents) in [
                ("[Content_Types].xml", "<Types/>"),
                ("ppt/presentation.xml", "<presentation/>"),
                ("ppt/slides/slide1.xml", "<slide/>"),
                ("ppt/charts/chart2.xml", &chart),
                ("ppt/diagrams/data1.xml", &diagram),
                ("ppt/diagrams/drawing3.xml", &drawing),
                ("ppt/diagrams/_rels/data1.xml.rels", rels),
                ("ppt/embeddings/data.xlsx", "original workbook"),
            ] {
                zip.start_file(name, SimpleFileOptions::default()).unwrap();
                zip.write_all(contents.as_bytes()).unwrap();
            }
            let doc = OfficeDocument::open_mode(zip.finish().unwrap().into_inner(), true).unwrap();
            let targets = doc
                .parts()
                .iter()
                .map(|part| {
                    part.paragraphs
                        .iter()
                        .map(|p| match p.plain_text().as_str() {
                            "Revenue by year" => Some("Yearly sales".into()),
                            "Annual revenue" => Some("Sales per year".into()),
                            "Plan ahead" if compatible => Some("Plan early".into()),
                            text => panic!("Unexpected translation unit: {text}"),
                        })
                        .collect()
                })
                .collect::<Vec<_>>();
            assert!(
                doc.unsupported
                    .iter()
                    .any(|r| r.part == "ppt/charts/chart2.xml")
            );
            assert_eq!(
                doc.unsupported
                    .iter()
                    .any(|r| r.part == "ppt/diagrams/data1.xml"),
                !compatible
            );
            let bytes = doc.write(TagFormat::Anchor, "en", &targets).unwrap();
            let mut zip = ZipArchive::new(Cursor::new(bytes)).unwrap();
            let mut contents = |name: &str| {
                let mut output = String::new();
                zip.by_name(name)
                    .unwrap()
                    .read_to_string(&mut output)
                    .unwrap();
                output
            };
            assert_eq!(
                contents("ppt/charts/chart2.xml")
                    .replace("Yearly sales", "Revenue by year")
                    .replace("Sales per year", "Annual revenue"),
                chart
            );
            assert_eq!(
                contents("ppt/diagrams/data1.xml"),
                if compatible {
                    diagram.replace("Plan ahead", "Plan early")
                } else {
                    diagram.clone()
                }
            );
            assert_eq!(
                contents("ppt/diagrams/drawing3.xml"),
                if compatible {
                    drawing.replace("Plan ahead", "Plan early")
                } else {
                    drawing.clone()
                }
            );
            assert_eq!(contents("ppt/diagrams/_rels/data1.xml.rels"), rels);
            assert_eq!(contents("ppt/embeddings/data.xlsx"), "original workbook");
        }
    }
}
