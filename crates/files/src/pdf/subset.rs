//! Sparse TrueType subsets. Keeping GIDs lets PDFium build its historical CID and
//! ToUnicode maps from the original cmap. Unused entries in loca have zero length.
use super::Error;
use std::collections::{BTreeMap, BTreeSet};
use ttf_parser::{Face, GlyphId, Tag};

fn word(bytes: &[u8], at: usize) -> Result<u16, Error> {
    let pair = bytes.get(at..at + 2).ok_or(Error::Font)?;
    Ok(u16::from_be_bytes([pair[0], pair[1]]))
}

fn table<'a>(face: &Face<'a>, tag: &[u8; 4]) -> Result<&'a [u8], Error> {
    face.raw_face()
        .table(Tag::from_bytes(tag))
        .ok_or(Error::Font)
}

fn offsets(face: &Face<'_>) -> Result<Vec<usize>, Error> {
    let loca = table(face, b"loca")?;
    let format = word(table(face, b"head")?, 50)?;
    let count = usize::from(face.number_of_glyphs()) + 1;
    match format {
        0 => (0..count)
            .map(|i| Ok(usize::from(word(loca, i * 2)?) * 2))
            .collect(),
        1 => loca.get(..count * 4).ok_or(Error::Font).map(|b| {
            b.as_chunks::<4>()
                .0
                .iter()
                .map(|v| u32::from_be_bytes(*v) as usize)
                .collect()
        }),
        _ => Err(Error::Font),
    }
}

fn components(data: &[u8], pending: &mut Vec<u16>) -> Result<(), Error> {
    if data.is_empty() || word(data, 0)? as i16 >= 0 {
        return Ok(());
    }
    let mut cursor = 10;
    loop {
        let flags = word(data, cursor)?;
        pending.push(word(data, cursor + 2)?);
        cursor += 4 + if flags & 1 != 0 { 4 } else { 2 };
        cursor += if flags & 0x80 != 0 {
            8
        } else if flags & 0x40 != 0 {
            4
        } else if flags & 8 != 0 {
            2
        } else {
            0
        };
        if cursor > data.len() {
            return Err(Error::Font);
        }
        if flags & 0x20 == 0 {
            return Ok(());
        }
    }
}

fn checksum(bytes: &[u8]) -> u32 {
    bytes.chunks(4).fold(0u32, |sum, chunk| {
        let mut buf = [0; 4];
        buf[..chunk.len()].copy_from_slice(chunk);
        sum.wrapping_add(u32::from_be_bytes(buf))
    })
}

fn prefix(head: &[u8], used: &BTreeSet<u16>) -> String {
    // Deterministic subset identifier over the original font checksum and GIDs.
    let mut hash = 2166136261u32;
    for b in head
        .iter()
        .copied()
        .chain(used.iter().flat_map(|g| g.to_be_bytes()))
    {
        hash = (hash ^ u32::from(b)).wrapping_mul(16777619);
    }
    (0..6)
        .map(|_| {
            let c = (b'A' + (hash % 26) as u8) as char;
            hash /= 26;
            c
        })
        .collect()
}

// PDFium uses the TrueType family name when creating the PDF font dictionaries.
// Keep localized names and licensing records, prefixing each identifying name.
fn names(data: &[u8], tag: &str) -> Result<Vec<u8>, Error> {
    if word(data, 0)? != 0 {
        return Err(Error::Font);
    }
    let count = usize::from(word(data, 2)?);
    let start = usize::from(word(data, 4)?);
    let mut records = data.get(..6 + 12 * count).ok_or(Error::Font)?.to_vec();
    let offset = u16::try_from(records.len()).map_err(|_| Error::Font)?;
    records[4..6].copy_from_slice(&offset.to_be_bytes());
    let mut strings = Vec::new();
    for record in records[6..].as_chunks_mut::<12>().0 {
        let platform = word(record, 0)?;
        let id = word(record, 6)?;
        let length = usize::from(word(record, 8)?);
        let at = start + usize::from(word(record, 10)?);
        let original = data.get(at..at + length).ok_or(Error::Font)?;
        let offset = strings.len();
        if matches!(id, 1 | 3 | 4 | 6 | 16 | 21) {
            if matches!(platform, 0 | 3) {
                strings.extend(tag.encode_utf16().flat_map(u16::to_be_bytes));
            } else {
                strings.extend_from_slice(tag.as_bytes());
            }
        }
        strings.extend_from_slice(original);
        for (at, n) in [(8, strings.len() - offset), (10, offset)] {
            record[at..at + 2]
                .copy_from_slice(&u16::try_from(n).map_err(|_| Error::Font)?.to_be_bytes());
        }
    }
    records.extend(strings);
    Ok(records)
}

pub(super) fn subset(
    face: &Face<'_>,
    glyphs: impl IntoIterator<Item = GlyphId>,
) -> Result<(String, Vec<u8>), Error> {
    let original = table(face, b"glyf")?;
    let locations = offsets(face)?;
    let glyph_data = |gid: u16| {
        let i = usize::from(gid);
        let end = *locations.get(i + 1).ok_or(Error::Font)?;
        original.get(locations[i]..end).ok_or(Error::Font)
    };
    let mut pending: Vec<_> = glyphs.into_iter().map(|g| g.0).collect();
    pending.push(0);
    let mut used = BTreeSet::new();
    while let Some(gid) = pending.pop() {
        if used.insert(gid) {
            components(glyph_data(gid)?, &mut pending)?;
        }
    }
    let mut glyf = Vec::new();
    let mut loca = Vec::new();
    for gid in 0..face.number_of_glyphs() {
        loca.extend_from_slice(&(glyf.len() as u32).to_be_bytes());
        if used.contains(&gid) {
            glyf.extend_from_slice(glyph_data(gid)?);
        }
    }
    loca.extend_from_slice(&(glyf.len() as u32).to_be_bytes());
    let mut head = table(face, b"head")?.to_vec();
    let tag = format!("{}+", prefix(&head, &used));
    head[8..12].fill(0);
    head[50..52].copy_from_slice(&1u16.to_be_bytes());
    let mut tables = BTreeMap::from([
        (*b"head", head),
        (*b"glyf", glyf),
        (*b"loca", loca),
        (*b"name", names(table(face, b"name")?, &tag)?),
    ]);
    // Preserve hinting programs and metrics, including the original cmap aliases.
    for tag in [
        b"OS/2", b"cmap", b"cvt ", b"fpgm", b"gasp", b"hhea", b"hmtx", b"maxp", b"post", b"prep",
        b"vhea", b"vmtx",
    ] {
        if let Some(data) = face.raw_face().table(Tag::from_bytes(tag)) {
            tables.insert(*tag, data.to_vec());
        }
    }
    let n = tables.len() as u16;
    let selector = n.ilog2() as u16;
    let search = 16 * (1u16 << selector);
    let mut out = vec![0, 1, 0, 0];
    for v in [n, search, selector, 16 * n - search] {
        out.extend(v.to_be_bytes());
    }
    out.resize(12 + usize::from(n) * 16, 0);
    let mut adjustment = 0;
    for (i, (tag, data)) in tables.into_iter().enumerate() {
        let start = out.len();
        let record = &mut out[12 + i * 16..28 + i * 16];
        record[..4].copy_from_slice(&tag);
        record[4..8].copy_from_slice(&checksum(&data).to_be_bytes());
        record[8..12].copy_from_slice(&(start as u32).to_be_bytes());
        record[12..16].copy_from_slice(&(data.len() as u32).to_be_bytes());
        if tag == *b"head" {
            adjustment = start + 8;
        }
        out.extend(data);
        out.resize(out.len().next_multiple_of(4), 0);
    }
    let sum = 0xb1b0afba_u32.wrapping_sub(checksum(&out));
    out[adjustment..adjustment + 4].copy_from_slice(&sum.to_be_bytes());
    Ok((tag, out))
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    #[ignore = "requires the bundled Noto Sans Regular font"]
    fn keeps_composite_components_metrics_and_unicode_gid_aliases() {
        let noto = super::super::Noto::load();
        let ipa = super::super::output_font().unwrap();
        let faces = super::super::Faces::new(&ipa, &noto, super::super::Family::Latin).unwrap();
        assert!(!faces.ipa_first);
        let original = &faces.fonts[0];
        let gid = original.glyph_index('é').unwrap();
        let (prefix, bytes) = subset(original, [gid]).unwrap();
        assert!(prefix[..6].bytes().all(|c| c.is_ascii_uppercase()) && prefix.ends_with('+'));
        assert_eq!(checksum(&bytes), 0xb1b0afba);
        let small = Face::parse(&bytes, 0).unwrap();
        for ch in ['é', 'e'] {
            let gid = original.glyph_index(ch).unwrap();
            assert_eq!(small.glyph_index(ch), Some(gid));
            assert_eq!(
                small.glyph_hor_advance(gid),
                original.glyph_hor_advance(gid)
            );
            assert!(original.glyph_bounding_box(gid).is_some());
            assert_eq!(
                small.glyph_bounding_box(gid),
                original.glyph_bounding_box(gid)
            );
        }
        assert!(
            small
                .glyph_bounding_box(original.glyph_index('Z').unwrap())
                .is_none()
        );
        assert!(bytes.len() < original.raw_face().data.len() / 2);
    }
}
