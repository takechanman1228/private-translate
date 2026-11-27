use std::collections::HashMap;
use std::io::{Read, Seek};

use quick_xml::{Reader, XmlVersion, events::Event};
use zip::ZipArchive;

use super::{Error, inheritance};

#[derive(Clone, Debug)]
pub struct FrameReview {
    pub part: String,
    /// Presentation order, not the numeric suffix of the slide XML filename.
    pub slide: Option<usize>,
    pub notes: bool,
    pub frame: usize,
    /// When no frame geometry is available, the index names a text paragraph.
    pub paragraph: bool,
    pub source: String,
    pub translated: String,
    pub scale_percent: u8,
    pub overflow: bool,
}

pub(super) fn slide_numbers<R: Read + Seek>(
    archive: &mut ZipArchive<R>,
) -> Result<HashMap<String, Vec<usize>>, Error> {
    let mut numbers = HashMap::new();
    let Some(rels) = inheritance::xml(archive, "ppt/_rels/presentation.xml.rels")? else {
        return Ok(numbers);
    };
    let mut targets = HashMap::new();
    let mut reader = Reader::from_str(&rels);
    loop {
        match reader.read_event().map_err(|_| Error::Xml)? {
            Event::Start(tag) | Event::Empty(tag)
                if tag.local_name().as_ref() == b"Relationship" =>
            {
                let attrs = tag
                    .attributes()
                    .map(|a| {
                        let a = a.map_err(|_| Error::Xml)?;
                        let value = a
                            .decoded_and_normalized_value(XmlVersion::Implicit1_0, reader.decoder())
                            .map_err(|_| Error::Xml)?
                            .into_owned();
                        Ok((a.key.as_ref().to_vec(), value))
                    })
                    .collect::<Result<HashMap<_, _>, Error>>()?;
                if attrs
                    .get(b"TargetMode".as_slice())
                    .is_some_and(|v| v == "External")
                    || !attrs
                        .get(b"Type".as_slice())
                        .is_some_and(|v| v.ends_with("/slide"))
                {
                    continue;
                }
                if let (Some(id), Some(target)) =
                    (attrs.get(b"Id".as_slice()), attrs.get(b"Target".as_slice()))
                {
                    let mut components = if target.starts_with('/') {
                        Vec::new()
                    } else {
                        vec!["ppt"]
                    };
                    for part in target.split('/') {
                        match part {
                            "" | "." => {}
                            ".." => {
                                if components.pop().is_none() {
                                    return Err(Error::Package);
                                }
                            }
                            _ => components.push(part),
                        }
                    }
                    targets.insert(id.clone(), components.join("/"));
                }
            }
            Event::DocType(_) => return Err(Error::Xml),
            Event::Eof => break,
            _ => {}
        }
    }
    let Some(presentation) = inheritance::xml(archive, "ppt/presentation.xml")? else {
        return Ok(numbers);
    };
    let mut reader = Reader::from_str(&presentation);
    let mut index = 0;
    loop {
        match reader.read_event().map_err(|_| Error::Xml)? {
            Event::Start(tag) | Event::Empty(tag) if tag.local_name().as_ref() == b"sldId" => {
                index += 1;
                for attr in tag.attributes() {
                    let attr = attr.map_err(|_| Error::Xml)?;
                    if attr.key.local_name().as_ref() == b"id" && attr.key.as_ref().contains(&b':')
                    {
                        let id = attr
                            .decoded_and_normalized_value(XmlVersion::Implicit1_0, reader.decoder())
                            .map_err(|_| Error::Xml)?;
                        if let Some(part) = targets.get(id.as_ref()) {
                            numbers.insert(part.clone(), vec![index]);
                            for relation in ["chart", "diagramData"] {
                                for (_, target) in
                                    inheritance::related_targets(archive, part, relation)?
                                {
                                    let slides = numbers.entry(target).or_default();
                                    if slides.last() != Some(&index) {
                                        slides.push(index);
                                    }
                                }
                            }
                            if let Some(notes) = inheritance::related(archive, part, "notesSlide")?
                            {
                                numbers.insert(notes, vec![index]);
                            }
                        }
                    }
                }
            }
            Event::DocType(_) => return Err(Error::Xml),
            Event::Eof => return Ok(numbers),
            _ => {}
        }
    }
}

#[cfg(test)]
mod tests {
    use crate::office::{OfficeDocument, TagFormat};
    use std::io::{Cursor, Write};
    use zip::{ZipWriter, write::SimpleFileOptions};

    #[test]
    fn review_uses_slide_order_and_the_same_fit_as_export() {
        let shape = |height: usize, font: usize, autofit: &str| {
            format!(
                r#"<sp><spPr><a:xfrm><a:ext cx="3810000" cy="{height}"/></a:xfrm></spPr><txBody><a:bodyPr lIns="0" rIns="0" tIns="0" bIns="0">{autofit}</a:bodyPr><a:p><a:r><a:rPr sz="{font}"/><a:t>Source label</a:t></a:r></a:p></txBody></sp>"#
            )
        };
        let xml = format!(
            r#"<root xmlns:a="http://schemas.openxmlformats.org/drawingml/2006/main">{}{}{}{}</root>"#,
            shape(1270000, 2000, ""),
            shape(254000, 2000, ""),
            shape(127000, 2000, "<a:spAutoFit/>"),
            shape(63500, 700, "")
        );
        let mut zip = ZipWriter::new(Cursor::new(Vec::new()));
        for (name, text) in [
            ("[Content_Types].xml", "<Types/>"),
            (
                "ppt/presentation.xml",
                r#"<p:presentation xmlns:p="p" xmlns:r="r"><p:sldIdLst><p:sldId id="101" r:id="second"/><p:sldId id="102" r:id="first"/></p:sldIdLst></p:presentation>"#,
            ),
            (
                "ppt/_rels/presentation.xml.rels",
                r#"<Relationships><Relationship Id="first" Type="r/slide" Target="slides/slide9.xml"/><Relationship Id="second" Type="r/slide" Target="/ppt/slides/slide2.xml"/></Relationships>"#,
            ),
            ("ppt/slides/slide9.xml", &xml),
            ("ppt/slides/slide2.xml", r#"<root/>"#),
            (
                "ppt/slides/_rels/slide9.xml.rels",
                r#"<Relationships><Relationship Id="notes" Type="r/notesSlide" Target="../notesSlides/notesSlide14.xml"/></Relationships>"#,
            ),
            ("ppt/notesSlides/notesSlide14.xml", r#"<root/>"#),
        ] {
            zip.start_file(name, SimpleFileOptions::default()).unwrap();
            zip.write_all(text.as_bytes()).unwrap();
        }
        let original = zip.finish().unwrap().into_inner();
        let document = OfficeDocument::open_mode(original, true).unwrap();
        assert_eq!(document.slide_numbers["ppt/slides/slide9.xml"], [2]);
        assert_eq!(document.slide_numbers["ppt/slides/slide2.xml"], [1]);
        assert_eq!(
            document.slide_numbers["ppt/notesSlides/notesSlide14.xml"],
            [2]
        );
        let targets = document
            .parts()
            .iter()
            .map(|part| {
                part.paragraphs
                    .iter()
                    .map(|_| Some("Translation & text".into()))
                    .collect()
            })
            .collect::<Vec<_>>();
        let reviews = document.powerpoint_layout_reviews(TagFormat::Anchor, &targets);
        assert_eq!(reviews.len(), 3);
        assert_eq!(reviews[0].slide, Some(2));
        assert_eq!(reviews[0].frame, 2);
        assert!(!reviews[0].overflow);
        assert_eq!(reviews[0].scale_percent, 83);
        assert_eq!(reviews[0].source, "Source label");
        assert_eq!(reviews[0].translated, "Translation & text");
        assert!(reviews[1].overflow);
        assert_eq!(reviews[1].scale_percent, 100);
        assert!(reviews[2].overflow);
        let output = document.write(TagFormat::Anchor, "en", &targets).unwrap();
        let reopened = OfficeDocument::open(output).unwrap();
        let slide = &reopened.parts()[0];
        let fonts = slide
            .paragraphs
            .iter()
            .map(|p| p.font_size.unwrap())
            .collect::<Vec<_>>();
        assert_eq!(fonts, [20.0, 16.6, 20.0, 7.0]);
        assert!(
            slide
                .paragraphs
                .iter()
                .all(|p| p.plain_text() == "Translation & text")
        );
        assert!(slide.xml.contains("<a:spAutoFit/>"));
        assert!(slide.xml.contains(r#"cx="3810000" cy="127000""#));
        let none = document
            .parts()
            .iter()
            .map(|part| vec![None; part.paragraphs.len()])
            .collect::<Vec<_>>();
        assert!(
            document
                .powerpoint_layout_reviews(TagFormat::Anchor, &none)
                .is_empty()
        );
    }
}
