//! Translation-only Type0 writer. CIDs identify Unicode scalars, never shared glyphs.
use super::fonts::Faces;
use super::{Error, inplace::internal};
use lopdf::{Document, Object, ObjectId, Stream, dictionary};
use std::collections::{BTreeMap, BTreeSet};

pub(super) struct TextLine {
    pub page: usize,
    pub text: String,
    pub size: f64,
    pub matrix: [f64; 6],
    pub color: [u32; 4],
}

impl TextLine {
    pub fn at(page: usize, x: f32, y: f32, size: f32, text: String) -> Self {
        Self {
            page,
            text,
            size: size.into(),
            matrix: [1., 0., 0., 1., x.into(), y.into()],
            color: [0, 0, 0, 255],
        }
    }
}

fn resources(doc: &Document, mut id: ObjectId) -> lopdf::Result<lopdf::Dictionary> {
    let mut seen = BTreeSet::new();
    while seen.insert(id) {
        let dict = doc.get_dictionary(id)?;
        if let Ok(value) = dict.get(b"Resources") {
            return doc.dereference(value)?.1.as_dict().cloned();
        }
        id = dict.get(b"Parent")?.as_reference()?;
    }
    Err(lopdf::Error::ReferenceCycle(id))
}

pub(super) fn append_faces(
    bytes: &[u8],
    faces: &Faces<'_>,
    lines: &[TextLine],
    added: bool,
) -> Result<Vec<u8>, Error> {
    if lines.iter().any(|l| l.color[3] <= 30) {
        return Err(internal());
    }
    if lines.is_empty() {
        return Ok(bytes.to_vec());
    }
    super::parser::with_document(bytes, |doc| write(doc, faces, lines, added))
        .map_err(|_| internal())
}

#[cfg(all(test, feature = "pdf-external-tests"))]
pub(super) fn append(bytes: &[u8], font: &[u8], lines: &[TextLine]) -> Result<Vec<u8>, Error> {
    let noto = super::fonts::Noto::default();
    let faces = Faces::new(font, &noto, super::Family::Ipa)?;
    append_faces(bytes, &faces, lines, false)
}

type WriteError = Box<dyn std::error::Error + Send + Sync>;

fn embed(
    doc: &mut Document,
    face: &ttf_parser::Face<'_>,
    cids: &BTreeMap<char, u16>,
    glyphs: &BTreeMap<char, ttf_parser::GlyphId>,
) -> Result<ObjectId, WriteError> {
    let (prefix, font) = super::subset::subset(face, glyphs.values().copied())?;
    let ps_name = face
        .names()
        .into_iter()
        .filter(|n| n.name_id == ttf_parser::name_id::POST_SCRIPT_NAME)
        .find_map(|n| n.to_string())
        .unwrap_or_else(|| "TranslationFont".into());
    let name = format!("{prefix}{ps_name}");
    let scale = 1000.0 / f64::from(face.units_per_em());
    let FontMaps { gids, widths, cmap } = font_maps(cids, |ch| {
        let glyph = glyphs[&ch];
        let width = face.glyph_hor_advance(glyph).ok_or("missing width")?;
        Ok((glyph.0, (f64::from(width) * scale) as f32))
    })?;
    let mut embedded = Stream::new(dictionary! {"Length1" => font.len() as i64}, font);
    embedded.compress()?;
    let font_file = doc.add_object(embedded);
    let bbox = face.global_bounding_box();
    let nums = |xs: &[f64]| Object::Array(xs.iter().map(|&x| Object::Real(x as f32)).collect());
    let descriptor = doc.add_object(dictionary!{
        "Type"=>"FontDescriptor", "FontName"=>name.as_str(), "Flags"=>4,
        "FontBBox"=>nums(&[bbox.x_min as f64*scale,bbox.y_min as f64*scale,bbox.x_max as f64*scale,bbox.y_max as f64*scale]),
        "ItalicAngle"=>0, "Ascent"=>Object::Real((face.ascender() as f64*scale) as f32),
        "Descent"=>Object::Real((face.descender() as f64*scale) as f32),
        "CapHeight"=>Object::Real((face.capital_height().unwrap_or(face.ascender()) as f64*scale) as f32),
        "StemV"=>80,"FontFile2"=>font_file
    });
    let gid_map = doc.add_object(Stream::new(dictionary! {}, gids));
    let unicode = doc.add_object(Stream::new(dictionary! {}, cmap.into_bytes()));
    let cid_font = doc.add_object(dictionary!{
        "Type"=>"Font", "Subtype"=>"CIDFontType2", "BaseFont"=>name.as_str(),
        "CIDSystemInfo"=>dictionary!{"Registry"=>Object::string_literal("Adobe"),"Ordering"=>Object::string_literal("Identity"),"Supplement"=>0},
        "FontDescriptor"=>descriptor,"CIDToGIDMap"=>gid_map,"DW"=>1000,
        "W"=>vec![Object::Integer(0),Object::Array(widths)]
    });
    let font_id = doc.add_object(dictionary!{"Type"=>"Font","Subtype"=>"Type0","BaseFont"=>name.as_str(),"Encoding"=>"Identity-H","DescendantFonts"=>vec![Object::Reference(cid_font)],"ToUnicode"=>unicode});
    Ok(font_id)
}

fn write(
    mut doc: Document,
    faces: &Faces<'_>,
    lines: &[TextLine],
    added: bool,
) -> Result<Vec<u8>, WriteError> {
    let mut by_char = BTreeMap::new();
    let mut glyphs = vec![BTreeMap::new(); faces.fonts.len()];
    for ch in lines
        .iter()
        .flat_map(|l| l.text.chars())
        .collect::<BTreeSet<_>>()
    {
        let (index, glyph) = faces
            .glyph(ch, added)
            .or_else(|| added.then(|| faces.exact('□').unwrap_or((0, ttf_parser::GlyphId(0)))))
            .ok_or("missing translation glyph")?;
        let cid = u16::try_from(glyphs[index].len() + 1)?;
        glyphs[index].insert(ch, glyph);
        by_char.insert(ch, (index, cid));
    }
    let mut ids = BTreeMap::new();
    for (index, glyphs) in glyphs
        .iter()
        .enumerate()
        .filter(|(_, group)| !group.is_empty())
    {
        let cids = glyphs.keys().map(|ch| (*ch, by_char[ch].1)).collect();
        ids.insert(index, embed(&mut doc, &faces.fonts[index], &cids, glyphs)?);
    }
    for (number, page) in doc.get_pages() {
        let here: Vec<_> = lines
            .iter()
            .filter(|l| l.page + 1 == number as usize)
            .collect();
        if here.is_empty() {
            continue;
        }
        let mut res = resources(&doc, page)?;
        let mut fonts = match res.get(b"Font") {
            Ok(o) => doc.dereference(o)?.1.as_dict()?.clone(),
            Err(_) => lopdf::Dictionary::new(),
        };
        let mut names = BTreeMap::new();
        for (&index, &id) in &ids {
            let mut name = format!("TranslationFont{index}");
            while fonts.has(name.as_bytes()) {
                name.push('_');
            }
            fonts.set(name.as_bytes(), id);
            names.insert(index, name);
        }
        res.set("Font", fonts);
        let mut states = match res.get(b"ExtGState") {
            Ok(o) => doc.dereference(o)?.1.as_dict()?.clone(),
            Err(_) => lopdf::Dictionary::new(),
        };
        let mut state_name = "TranslationState".to_owned();
        while states.has(state_name.as_bytes()) {
            state_name.push('_');
        }
        let mut content = String::new();
        for (index, line) in here.into_iter().enumerate() {
            let mut key = format!("{state_name}{index}");
            while states.has(key.as_bytes()) {
                key.push('_');
            }
            // A fresh graphics state removes inherited transparency/blend settings.
            states.set(key.as_bytes(), dictionary!{"Type"=>"ExtGState","ca"=>Object::Real(line.color[3] as f32/255.0),"CA"=>1,"BM"=>"Normal","SMask"=>"None"});
            content.push_str(&format!("q /{key} gs BT 0 Tc 0 Tw 100 Tz 0 Ts 0 Tr\n"));
            content.push_str(&format!(
                "{} {} {} rg\n",
                line.color[0] as f64 / 255.0,
                line.color[1] as f64 / 255.0,
                line.color[2] as f64 / 255.0
            ));
            for v in line.matrix {
                content.push_str(&format!("{v} "));
            }
            content.push_str("Tm ");
            let chars: Vec<_> = line.text.chars().collect();
            let mut start = 0;
            while start < chars.len() {
                let index = by_char[&chars[start]].0;
                let face = &faces.fonts[index];
                content.push_str(&format!("/{} {} Tf ", names[&index], line.size));
                let supplementary = chars[start] as u32 > 0xffff;
                let end = (start + 1..chars.len())
                    .find(|&i| {
                        by_char[&chars[i]].0 != index || (chars[i] as u32 > 0xffff) != supplementary
                    })
                    .unwrap_or(chars.len());
                let segment: String = chars[start..end].iter().collect();
                // Poppler normalizes NBSP without ActualText. PDFium drops supplementary
                // scalars in ActualText, so those always use their ToUnicode mapping.
                let actual = !supplementary
                    && (segment.contains('\u{a0}')
                        || segment.chars().any(|ch| face.glyph_index(ch).is_none()));
                if actual {
                    content.push_str("/Span << /ActualText <FEFF");
                    for unit in segment.encode_utf16() {
                        content.push_str(&format!("{unit:04X}"));
                    }
                    content.push_str("> >> BDC ");
                }
                content.push('<');
                for ch in segment.chars() {
                    content.push_str(&format!("{:04X}", by_char[&ch].1));
                }
                content.push_str("> Tj ");
                if actual {
                    content.push_str("EMC ");
                }
                start = end;
            }
            // Text only: no path is drawn with a translation.
            content.push_str("ET\nQ\n");
        }
        res.set("ExtGState", states);
        doc.get_object_mut(page)?
            .as_dict_mut()?
            .set("Resources", res);
        // Isolate original graphics state, including a nonidentity CTM or clipping path.
        let mut contents: Vec<Object> = doc
            .get_page_contents(page)
            .into_iter()
            .map(Object::Reference)
            .collect();
        let prefix = doc.add_object(Stream::new(dictionary! {}, b"q\n".to_vec()));
        let suffix = doc.add_object(Stream::new(
            dictionary! {},
            format!("\nQ\n{content}").into_bytes(),
        ));
        contents.insert(0, prefix.into());
        contents.push(suffix.into());
        doc.get_object_mut(page)?
            .as_dict_mut()?
            .set("Contents", contents);
    }
    let mut output = Vec::new();
    doc.save_to(&mut output)?;
    Ok(output)
}

struct FontMaps {
    gids: Vec<u8>,
    widths: Vec<Object>,
    cmap: String,
}

fn font_maps(
    cids: &BTreeMap<char, u16>,
    lookup: impl Fn(char) -> Result<(u16, f32), Box<dyn std::error::Error + Send + Sync>>,
) -> Result<FontMaps, Box<dyn std::error::Error + Send + Sync>> {
    let mut gids = vec![0u8; (cids.len() + 1) * 2];
    let mut widths = vec![Object::Integer(0)];
    let mut cmap = String::from(
        "/CIDInit /ProcSet findresource begin 12 dict begin begincmap\n/CIDSystemInfo << /Registry (Adobe) /Ordering (UCS) /Supplement 0 >> def\n/CMapName /TranslationUnicode def /CMapType 2 def\n1 begincodespacerange <0000> <FFFF> endcodespacerange\n",
    );
    let entries: Vec<_> = cids.iter().collect();
    for chunk in entries.chunks(100) {
        cmap.push_str(&format!("{} beginbfchar\n", chunk.len()));
        for &(ch, &cid) in chunk {
            let (glyph, width) = lookup(*ch)?;
            gids[usize::from(cid) * 2..usize::from(cid) * 2 + 2]
                .copy_from_slice(&glyph.to_be_bytes());
            widths.push(Object::Real(width));
            let hex: String = ch
                .encode_utf16(&mut [0; 2])
                .iter()
                .map(|u| format!("{u:04X}"))
                .collect();
            cmap.push_str(&format!("<{cid:04X}> <{hex}>\n"));
        }
        cmap.push_str("endbfchar\n");
    }
    cmap.push_str("endcmap CMapName currentdict /CMap defineresource pop end end\n");
    Ok(FontMaps { gids, widths, cmap })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn unicode_blocks_widths_and_glyph_map_cover_every_cid() {
        let chars = (0x20..0x20 + 204)
            .map(|c| char::from_u32(c).unwrap())
            .chain(['𩿎', '𩿗']);
        let cids: BTreeMap<_, _> = chars
            .enumerate()
            .map(|(i, c)| (c, (i + 1) as u16))
            .collect();
        // Both astral scalars deliberately share a glyph; they must keep separate CIDs.
        let maps = font_maps(&cids, |_| Ok((123, 500.0))).unwrap();
        assert_eq!(maps.gids.len(), 2 * (cids.len() + 1));
        assert_eq!(&maps.gids[..2], &[0, 0]);
        assert!(
            maps.gids[2..]
                .as_chunks::<2>()
                .0
                .iter()
                .all(|x| *x == 123u16.to_be_bytes())
        );
        assert_eq!(maps.widths.len(), cids.len() + 1);
        assert_eq!(maps.widths[0].as_i64().unwrap(), 0);
        assert!(
            maps.widths[1..]
                .iter()
                .all(|x| x.as_f32().unwrap() == 500.0)
        );
        let counts: Vec<usize> = maps
            .cmap
            .lines()
            .filter_map(|l| l.strip_suffix(" beginbfchar"))
            .map(|n| n.parse().unwrap())
            .collect();
        assert_eq!(counts, [100, 100, 6]);
        let mut actual_count = 0;
        let mut declared = 0;
        for line in maps.cmap.lines() {
            if let Some(n) = line.strip_suffix(" beginbfchar") {
                declared = n.parse().unwrap();
                actual_count = 0;
                assert!(declared <= 100);
            } else if line == "endbfchar" {
                assert_eq!(actual_count, declared);
            } else if line.starts_with('<') {
                actual_count += 1;
            }
        }
        for (ch, hex) in [('𩿎', "D867DFCE"), ('𩿗', "D867DFD7")] {
            assert!(maps.cmap.contains(&format!("<{:04X}> <{hex}>", cids[&ch])));
        }
    }
}
