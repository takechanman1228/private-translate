//! Resolve table text defaults through the package's own slide/master links.
use super::{Error, MAX_XML, Paragraph, symbols};
use quick_xml::{events::Event, reader::Reader};
use std::io::{Read, Seek};
use zip::ZipArchive;

pub(super) fn xml<R: Read + Seek>(
    archive: &mut ZipArchive<R>,
    name: &str,
) -> Result<Option<String>, Error> {
    let Some(index) = archive.index_for_name(name) else {
        return Ok(None);
    };
    let file = archive.by_index(index)?;
    if file.size() > MAX_XML {
        return Err(Error::Limit);
    }
    let mut text = String::new();
    file.take(MAX_XML + 1).read_to_string(&mut text)?;
    if text.len() as u64 > MAX_XML {
        return Err(Error::Limit);
    }
    Ok(Some(text))
}

pub(super) fn related<R: Read + Seek>(
    archive: &mut ZipArchive<R>,
    part: &str,
    kind: &str,
) -> Result<Option<String>, Error> {
    related_id(archive, part, kind, None)
}

pub(super) fn related_id<R: Read + Seek>(
    archive: &mut ZipArchive<R>,
    part: &str,
    kind: &str,
    id: Option<&str>,
) -> Result<Option<String>, Error> {
    Ok(related_targets(archive, part, kind)?
        .into_iter()
        .find(|(candidate, _)| id.is_none_or(|id| id == candidate))
        .map(|(_, target)| target))
}

pub(super) fn related_targets<R: Read + Seek>(
    archive: &mut ZipArchive<R>,
    part: &str,
    kind: &str,
) -> Result<Vec<(String, String)>, Error> {
    let Some((dir, file)) = part.rsplit_once('/') else {
        return Ok(Vec::new());
    };
    let Some(text) = xml(archive, &format!("{dir}/_rels/{file}.rels"))? else {
        return Ok(Vec::new());
    };
    let mut reader = Reader::from_str(&text);
    let mut result = Vec::new();
    loop {
        match reader.read_event().map_err(|_| Error::Xml)? {
            Event::Empty(tag) | Event::Start(tag)
                if tag.local_name().as_ref() == b"Relationship" =>
            {
                let attrs = tag
                    .attributes()
                    .collect::<Result<Vec<_>, _>>()
                    .map_err(|_| Error::Xml)?;
                let value = |key: &[u8]| {
                    attrs
                        .iter()
                        .find(|a| a.key.as_ref() == key)
                        .map(|a| a.value.as_ref())
                };
                if value(b"TargetMode") == Some(b"External")
                    || !value(b"Type").is_some_and(|t| t.ends_with(format!("/{kind}").as_bytes()))
                {
                    continue;
                }
                let Some(target) = value(b"Target") else {
                    continue;
                };
                let target = std::str::from_utf8(target).map_err(|_| Error::Xml)?;
                let mut components: Vec<&str> = if target.starts_with('/') {
                    Vec::new()
                } else {
                    dir.split('/').collect()
                };
                for c in target.split('/') {
                    match c {
                        "" | "." => {}
                        ".." => {
                            if components.pop().is_none() {
                                return Err(Error::Package);
                            }
                        }
                        _ => components.push(c),
                    }
                }
                let id = value(b"Id").unwrap_or_default();
                result.push((
                    String::from_utf8_lossy(id).into_owned(),
                    components.join("/"),
                ));
            }
            Event::DocType(_) => return Err(Error::Xml),
            Event::Eof => return Ok(result),
            _ => {}
        }
    }
}

fn levels(text: &str, container: &[u8]) -> Result<[Option<f64>; 9], Error> {
    let mut result = [None; 9];
    let mut reader = Reader::from_str(text);
    let mut stack: Vec<Vec<u8>> = Vec::new();
    loop {
        let event = reader.read_event().map_err(|_| Error::Xml)?;
        match &event {
            Event::Start(tag) | Event::Empty(tag) => {
                let name = tag.local_name();
                if name.as_ref() == b"defRPr"
                    && stack.iter().any(|n| n == container)
                    && let Some(level) = stack.last().and_then(|n| {
                        (1..=9).find(|i| n.as_slice() == format!("lvl{i}pPr").as_bytes())
                    })
                {
                    let raw = format!("<{} />", std::str::from_utf8(tag).map_err(|_| Error::Xml)?);
                    result[level - 1] = super::layout::font_points(&raw);
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
    Ok(result)
}

type Levels = [Option<f64>; 9];

struct Placeholder {
    frame: super::layout::Frame,
    fonts: Levels,
    faces: symbols::Faces,
}

fn placeholders(text: &str, has_symbols: bool) -> Result<Vec<Placeholder>, Error> {
    super::layout::frames(text)?
        .into_iter()
        .filter(|f| f.placeholder.is_some())
        .map(|frame| {
            let fonts = levels(&text[frame.range.clone()], b"lstStyle")?;
            let faces = if has_symbols {
                symbols::faces(&text[frame.range.clone()], b"lstStyle")?
            } else {
                Default::default()
            };
            Ok(Placeholder {
                frame,
                fonts,
                faces,
            })
        })
        .collect()
}

fn master_type(kind: &str) -> &str {
    match kind {
        "ctrTitle" => "title",
        "subTitle" | "obj" => "body",
        kind => kind,
    }
}

fn layout_placeholder<'a>(
    layouts: &'a [Placeholder],
    idx: usize,
    kind: &str,
) -> Option<&'a Placeholder> {
    let mut candidates = layouts.iter().filter_map(|layout| {
        let (parent_idx, parent_kind) = layout.frame.placeholder.as_ref()?;
        (*parent_idx == idx).then_some((layout, parent_kind))
    });
    candidates
        .clone()
        .find(|(_, k)| k.as_str() == kind)
        .or_else(|| candidates.find(|(_, k)| master_type(k) == master_type(kind)))
        .map(|(layout, _)| layout)
}

fn style(kind: &str) -> usize {
    match kind {
        "title" | "ctrTitle" => 0,
        "body" | "obj" | "subTitle" => 1,
        _ => 2,
    }
}

pub(super) fn paragraph_level(p: &Paragraph) -> usize {
    let mut reader = Reader::from_str(&p.before);
    loop {
        match reader.read_event() {
            Ok(Event::Start(tag) | Event::Empty(tag)) if tag.local_name().as_ref() == b"pPr" => {
                return tag
                    .attributes()
                    .flatten()
                    .find(|a| a.key.as_ref() == b"lvl")
                    .and_then(|a| std::str::from_utf8(&a.value).ok()?.parse().ok())
                    .unwrap_or(0);
            }
            Ok(Event::Eof) | Err(_) => return 0,
            _ => {}
        }
    }
}

pub(super) fn resolve_frames<R: Read + Seek>(
    archive: &mut ZipArchive<R>,
    part: &str,
    source: &str,
    frames: &mut [super::layout::Frame],
    paragraphs: &mut [Paragraph],
) -> Result<std::collections::HashMap<String, String>, Error> {
    if !part.starts_with("ppt/slides/") {
        return Ok(Default::default());
    }
    let mut layouts = Vec::new();
    let mut masters = Vec::new();
    let mut styles = [[None; 9]; 3];
    let has_symbols = source.contains("sym");
    let mut faces: [symbols::Faces; 3] = Default::default();
    let mut theme = std::collections::HashMap::new();
    if let Some(layout) = related(archive, part, "slideLayout")? {
        if let Some(text) = xml(archive, &layout)? {
            layouts = placeholders(&text, has_symbols)?;
        }
        if let Some(master) = related(archive, &layout, "slideMaster")?
            && let Some(text) = xml(archive, &master)?
        {
            masters = placeholders(&text, has_symbols)?;
            if has_symbols {
                faces = [
                    symbols::faces(&text, b"titleStyle")?,
                    symbols::faces(&text, b"bodyStyle")?,
                    symbols::faces(&text, b"otherStyle")?,
                ];
                for (part, kind) in [
                    (master.as_str(), "theme"),
                    (layout.as_str(), "themeOverride"),
                    (part, "themeOverride"),
                ] {
                    if let Some(name) = related(archive, part, kind)?
                        && let Some(value) = xml(archive, &name)?
                    {
                        theme.extend(symbols::theme_fonts(&value)?);
                    }
                }
            }
            styles = [
                levels(&text, b"titleStyle")?,
                levels(&text, b"bodyStyle")?,
                levels(&text, b"otherStyle")?,
            ];
        }
    }
    let presentation = xml(archive, "ppt/presentation.xml")?.unwrap_or_default();
    let fallback = levels(&presentation, b"defaultTextStyle")?;
    let fallback_ea = if has_symbols {
        symbols::faces(&presentation, b"defaultTextStyle")?
    } else {
        Default::default()
    };
    for frame in frames {
        // Group child coordinates can have independent x/y scaling. When no
        // run size is explicit, materializing a master size into the group can
        // enlarge text over adjacent diagram labels. Leave that fit for review.
        if frame.grouped && frame.placeholder.is_none() && !frame.is_table_cell() {
            continue;
        }
        let mut category = 2;
        let mut local = levels(&source[frame.range.clone()], b"lstStyle")?;
        let mut local_ea = if has_symbols {
            symbols::faces(&source[frame.range.clone()], b"lstStyle")?
        } else {
            Default::default()
        };
        if let Some((idx, kind)) = &frame.placeholder {
            let layout = layout_placeholder(&layouts, *idx, kind);
            let kind = layout
                .and_then(|p| p.frame.placeholder.as_ref())
                .map_or(kind.as_str(), |ph| ph.1.as_str());
            category = style(kind);
            let master = masters.iter().find(|p| {
                p.frame
                    .placeholder
                    .as_ref()
                    .is_some_and(|ph| ph.1 == master_type(kind))
            });
            frame.title = category == 0;
            for parent in [layout, master].into_iter().flatten() {
                frame.inherit(&parent.frame);
                for (f, inherited) in local_ea.iter_mut().zip(&parent.faces) {
                    if f.is_none() {
                        f.clone_from(inherited);
                    }
                }
                for (size, inherited) in local.iter_mut().zip(parent.fonts) {
                    *size = size.or(inherited);
                }
            }
        }
        let start = paragraphs.partition_point(|p| p.body.start < frame.range.start);
        let end = paragraphs.partition_point(|p| p.body.start < frame.range.end);
        for p in &mut paragraphs[start..end] {
            let level = paragraph_level(p);
            if level >= 9 {
                continue;
            }
            if has_symbols
                && let Some(f) = local_ea[level]
                    .as_ref()
                    .or(faces[category][level].as_ref())
                    .or(fallback_ea[level].as_ref())
            {
                p.inherited_ea = Some(f.clone());
            }
            let inherits_size = |run: &super::Run| {
                run.props
                    .as_ref()
                    .and_then(|r| super::layout::font_points(&run.write_before[r.clone()]))
                    .is_none()
            };
            if p.font_size.is_some()
                && !inherits_size(&p.base)
                && !p
                    .tags
                    .iter()
                    .any(|t| matches!(t,super::Tag::Style(r) if inherits_size(r)))
            {
                continue;
            }
            // A paragraph's default run size precedes the list/master styles.
            let mut reader = Reader::from_str(&p.before);
            let mut paragraph_default = None;
            loop {
                match reader.read_event() {
                    Ok(Event::Start(tag) | Event::Empty(tag))
                        if tag.local_name().as_ref() == b"defRPr" =>
                    {
                        paragraph_default = super::layout::font_points(&format!(
                            "<{} />",
                            std::str::from_utf8(&tag).map_err(|_| Error::Xml)?
                        ));
                        break;
                    }
                    Ok(Event::Eof) | Err(_) => break,
                    _ => {}
                }
            }
            if let Some(size) = paragraph_default
                .or(local[level])
                .or(styles[category][level])
                .or_else(|| {
                    // A master can leave a level size to the renderer. Using the
                    // presentation default here can enlarge a body sublevel.
                    (frame.placeholder.is_none() || styles[category].iter().all(Option::is_none))
                        .then_some(fallback[level])
                        .flatten()
                })
            {
                p.inherited_font_size = Some(size);
                p.font_size = Some(p.font_size.unwrap_or(size).max(size));
            }
        }
    }
    Ok(theme)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::{Cursor, Write};
    use zip::{ZipWriter, write::SimpleFileOptions};

    #[test]
    fn follows_the_slides_master_and_keeps_explicit_sizes() {
        let slide = r#"<root xmlns:a="http://schemas.openxmlformats.org/drawingml/2006/main"><graphicFrame><a:tbl><a:tblGrid><a:gridCol w="1270000"/></a:tblGrid><a:tr h="254000"><a:tc><a:txBody><a:bodyPr/><a:lstStyle/><a:p><a:r><a:rPr/><a:t>短文</a:t></a:r></a:p><a:p><a:r><a:rPr sz="900"/><a:t>別の文字</a:t></a:r></a:p></a:txBody><a:tcPr/></a:tc></a:tr></a:tbl></graphicFrame></root>"#;
        let mut writer = ZipWriter::new(Cursor::new(Vec::new()));
        for (name, text) in [
            ("[Content_Types].xml", "<Types/>"),
            ("ppt/presentation.xml", "<presentation/>"),
            ("ppt/slides/slide1.xml", slide),
            (
                "ppt/slides/_rels/slide1.xml.rels",
                r#"<Relationships><Relationship Type="x/slideLayout" Target="../slideLayouts/slideLayout2.xml"/></Relationships>"#,
            ),
            (
                "ppt/slideLayouts/_rels/slideLayout2.xml.rels",
                r#"<Relationships><Relationship Type="x/slideMaster" Target="../slideMasters/slideMaster2.xml"/></Relationships>"#,
            ),
            (
                "ppt/slideMasters/slideMaster2.xml",
                r#"<master><otherStyle><lvl1pPr><defRPr sz="1300"/></lvl1pPr></otherStyle></master>"#,
            ),
        ] {
            writer
                .start_file(name, SimpleFileOptions::default())
                .unwrap();
            writer.write_all(text.as_bytes()).unwrap();
        }
        let bytes = writer.finish().unwrap().into_inner();
        let deck = super::super::OfficeDocument::open_mode(bytes.clone(), true).unwrap();
        let paragraphs = &deck.parts()[0].paragraphs;
        assert_eq!(paragraphs[0].font_size, Some(13.0));
        assert_eq!(paragraphs[1].font_size, Some(9.0));
        assert_eq!(paragraphs[1].inherited_font_size, None);
        assert_eq!(
            deck.write(super::super::TagFormat::Anchor, "en", &[vec![None, None]])
                .unwrap(),
            bytes
        );
    }

    #[test]
    fn title_uses_layout_bounds_and_its_own_master_style() {
        let slide = r#"<root xmlns:a="http://schemas.openxmlformats.org/drawingml/2006/main"><sp><nvSpPr><nvPr><ph type="title"/></nvPr></nvSpPr><txBody><a:bodyPr/><a:p><a:r><a:t>Library</a:t></a:r></a:p></txBody></sp><sp><nvSpPr><nvPr><ph type="body" idx="1"/></nvPr></nvSpPr><txBody><a:bodyPr/><a:p><a:pPr lvl="1"/><a:r><a:t>Desk</a:t></a:r></a:p></txBody></sp></root>"#;
        let layout = r#"<root><sp><ph type="title"/><spPr><xfrm><off x="0" y="0"/><ext cx="2540000" cy="508000"/></xfrm></spPr><txBody><bodyPr wrap="none"/></txBody></sp></root>"#;
        let mut zip = ZipWriter::new(Cursor::new(Vec::new()));
        for (part, xml) in [
            ("[Content_Types].xml", "<Types/>"),
            (
                "ppt/presentation.xml",
                r#"<presentation><defaultTextStyle><lvl2pPr><defRPr sz="2400"/></lvl2pPr></defaultTextStyle></presentation>"#,
            ),
            ("ppt/slides/slide1.xml", slide),
            (
                "ppt/slides/_rels/slide1.xml.rels",
                r#"<rels><Relationship Type="r/slideLayout" Target="../slideLayouts/layout.xml"/></rels>"#,
            ),
            ("ppt/slideLayouts/layout.xml", layout),
            (
                "ppt/slideLayouts/_rels/layout.xml.rels",
                r#"<rels><Relationship Type="r/slideMaster" Target="../slideMasters/master.xml"/></rels>"#,
            ),
            (
                "ppt/slideMasters/master.xml",
                r#"<master><titleStyle><lvl1pPr><defRPr sz="3200"/></lvl1pPr></titleStyle><bodyStyle><lvl1pPr><defRPr sz="1400"/></lvl1pPr><lvl2pPr><defRPr/></lvl2pPr></bodyStyle></master>"#,
            ),
        ] {
            zip.start_file(part, SimpleFileOptions::default()).unwrap();
            zip.write_all(xml.as_bytes()).unwrap();
        }
        let doc = super::super::OfficeDocument::open_mode(zip.finish().unwrap().into_inner(), true)
            .unwrap();
        let p = &doc.parts()[0].paragraphs[0];
        assert_eq!(p.inherited_font_size, Some(32.0));
        // Missing body-level size must remain native. The 24 pt presentation
        // default can be larger than the renderer's actual second-level size.
        assert_eq!(doc.parts()[0].paragraphs[1].inherited_font_size, None);
        let reviews = doc.powerpoint_layout_reviews(
            super::super::TagFormat::Anchor,
            &[vec![Some("図書室のご案内".into()), None]],
        );
        assert_eq!(reviews.len(), 1);
        assert!(!reviews[0].overflow);
        assert!((80..100).contains(&reviews[0].scale_percent));
    }

    #[test]
    fn layout_matches_both_defaulted_attributes_and_prefers_an_exact_role() {
        let xml = r#"<root><sp><ph type="title"/><txBody/></sp><sp><ph idx="4"/><txBody/></sp><sp><ph type="subTitle" idx="4"/><txBody/></sp></root>"#;
        let layouts = placeholders(xml, false).unwrap();
        assert!(layout_placeholder(&layouts, 0, "body").is_none());
        for (idx, kind, expected) in [
            (0, "ctrTitle", "title"),
            (4, "body", "obj"),
            (4, "subTitle", "subTitle"),
        ] {
            let p = layout_placeholder(&layouts, idx, kind).unwrap();
            assert_eq!(p.frame.placeholder.as_ref().unwrap().1, expected);
        }
        assert!(layout_placeholder(&layouts, 5, "obj").is_none());
        assert!(layout_placeholder(&layouts, 4, "dt").is_none());
    }

    #[test]
    fn list_levels_do_not_leak_between_styles() {
        let text = r#"<root><otherStyle><lvl1pPr><defRPr sz="1300"/></lvl1pPr><lvl2pPr><defRPr sz="1100"/></lvl2pPr></otherStyle><titleStyle><lvl1pPr><defRPr sz="4000"/></lvl1pPr></titleStyle></root>"#;
        let sizes = levels(text, b"otherStyle").unwrap();
        assert_eq!(sizes[0], Some(13.0));
        assert_eq!(sizes[1], Some(11.0));
        assert_eq!(sizes[2], None);
    }
}
