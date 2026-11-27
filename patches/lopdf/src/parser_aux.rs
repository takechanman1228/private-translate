use log::warn;

use crate::{Dictionary, Object, ObjectId, Stream, parser};
use crate::{
    Error, Result,
    content::{Content, Operation},
    document::Document,
    encodings::Encoding,
    error::ParseError,
    object::Object::Name,
    xref::{Xref, XrefEntry, XrefType},
};
use std::{
    collections::BTreeMap,
    io::{Cursor, Read},
};

impl Content<Vec<Operation>> {
    /// Decode content operations.
    pub fn decode(data: &[u8]) -> Result<Self> {
        parser::content(data).ok_or(ParseError::InvalidContentStream.into())
    }

    /// Decode content operations one at a time without collecting them.
    ///
    /// Yields the operations [`Content::decode`] returns, or its error at the point
    /// where the decode fails.
    pub fn decode_iter(data: &[u8]) -> parser::ContentOperations<'_> {
        parser::ContentOperations::new(data)
    }

    /// Strict decode content operations.
    pub fn decode_strict(data: &[u8]) -> Result<Self> {
        parser::content_strict(data).map_err(|e| e.into())
    }
}

impl Stream {
    /// Decode content after decoding all stream filters.
    pub fn decode_content(&self) -> Result<Content<Vec<Operation>>> {
        Content::decode(&self.content)
    }
}

impl Document {
    /// Get decoded page content;
    pub fn get_and_decode_page_content(&self, page_id: ObjectId) -> Result<Content<Vec<Operation>>> {
        let content_data = self.get_page_content(page_id);
        Content::decode(&content_data)
    }

    /// Add content to a page. All existing content will be unchanged.
    pub fn add_to_page_content(&mut self, page_id: ObjectId, content: Content<Vec<Operation>>) -> Result<()> {
        let content_data = Content::encode(&content)?;
        self.add_page_contents(page_id, content_data)?;
        Ok(())
    }

    pub fn extract_text(&self, page_numbers: &[u32]) -> Result<String> {
        self.extract_text_inner(page_numbers, None)
    }

    /// Extract text from the given pages, bounding the total decompressed content
    /// of each page to `max_decompressed_size` bytes.
    ///
    /// This is the decompression-bomb-safe counterpart to
    /// [`Document::extract_text`]: page content is decoded via
    /// [`Document::get_page_content_with_limit`], so a small compressed content
    /// stream cannot inflate without limit. Prefer it over [`Document::extract_text`]
    /// for PDFs from an untrusted source. Returns
    /// [`DecompressError::MemoryLimitExceeded`](crate::DecompressError::MemoryLimitExceeded)
    /// if any requested page's content would exceed the limit.
    pub fn extract_text_with_limit(&self, page_numbers: &[u32], max_decompressed_size: usize) -> Result<String> {
        self.extract_text_inner(page_numbers, Some(max_decompressed_size))
    }

    fn extract_text_inner(&self, page_numbers: &[u32], limit: Option<usize>) -> Result<String> {
        let text_fragments = self.extract_text_chunks_inner(page_numbers, limit);
        let mut text = String::new();
        for maybe_text_fragment in text_fragments.into_iter() {
            let text_fragment = maybe_text_fragment?;
            text.push_str(&text_fragment);
        }

        Ok(text)
    }

    pub fn extract_text_chunks(&self, page_numbers: &[u32]) -> Vec<Result<String>> {
        self.extract_text_chunks_inner(page_numbers, None)
    }

    /// Bomb-safe counterpart to [`Document::extract_text_chunks`], bounding the
    /// total decompressed content of each page to `max_decompressed_size` bytes.
    /// A page whose content exceeds the limit yields an `Err` chunk carrying
    /// [`DecompressError::MemoryLimitExceeded`](crate::DecompressError::MemoryLimitExceeded).
    pub fn extract_text_chunks_with_limit(
        &self, page_numbers: &[u32], max_decompressed_size: usize,
    ) -> Vec<Result<String>> {
        self.extract_text_chunks_inner(page_numbers, Some(max_decompressed_size))
    }

    fn extract_text_chunks_inner(&self, page_numbers: &[u32], limit: Option<usize>) -> Vec<Result<String>> {
        let pages: BTreeMap<u32, (u32, u16)> = self.get_pages();
        page_numbers
            .iter()
            .flat_map(|page_number| {
                let result = self.extract_text_chunks_from_page(&pages, *page_number, limit);
                match result {
                    Ok(text_chunks) => text_chunks,
                    Err(err) => vec![Err(err)],
                }
            })
            .collect()
    }

    fn extract_text_chunks_from_page(
        &self, pages: &BTreeMap<u32, (u32, u16)>, page_number: u32, limit: Option<usize>,
    ) -> Result<Vec<Result<String>>> {
        let mut collected_chunks_and_errs: Vec<std::result::Result<String, Error>> = Vec::new();

        let page_id = *pages.get(&page_number).ok_or(Error::PageNumberNotFound(page_number))?;
        let fonts = self.get_page_fonts(page_id)?;
        let encodings: BTreeMap<Vec<u8>, Encoding> = fonts
            .into_iter()
            .filter_map(|(name, font)| {
                let encoding = match limit {
                    Some(max) => font.get_font_encoding_with_limit(self, max),
                    None => font.get_font_encoding(self),
                };
                match encoding {
                    Ok(it) => Some((name, it)),
                    Err(err) => {
                        collected_chunks_and_errs.push(Err(err));
                        None
                    }
                }
            })
            .collect();
        let content_data = match limit {
            Some(max) => self.get_page_content_with_limit(page_id, max)?,
            None => self.get_page_content(page_id),
        };
        let content = Content::decode(&content_data)?;

        // each text with different encoding is extracted as separate chunk
        let mut current_encoding = None;
        let mut current_text = String::new();
        for operation in &content.operations {
            match operation.operator.as_ref() {
                "Tf" => {
                    let current_font = operation
                        .operands
                        .first()
                        .ok_or_else(|| Error::Syntax("missing font operand".to_string()))?
                        .as_name();
                    current_encoding = match current_font {
                        Ok(font) => encodings.get(font),
                        Err(err) => {
                            collected_chunks_and_errs.push(Err(err));
                            None
                        }
                    };

                    if !current_text.is_empty() {
                        collected_chunks_and_errs.push(Ok(current_text));
                        current_text = String::new();
                    }
                }
                "Tj" | "TJ" => match current_encoding {
                    Some(encoding) => {
                        let res = collect_text(&mut current_text, encoding, &operation.operands);
                        if let Err(err) = res {
                            collected_chunks_and_errs.push(Err(err));
                        }
                    }
                    None => warn!("Could not decode extracted text"),
                },
                // PDF 32000-1 §9.4.3: `'` is `T* Tj` — move to the next line, then show
                // the single string operand.
                "'" => match current_encoding {
                    Some(encoding) => {
                        if !current_text.ends_with('\n') {
                            current_text.push('\n');
                        }
                        let res = collect_text(&mut current_text, encoding, &operation.operands);
                        if let Err(err) = res {
                            collected_chunks_and_errs.push(Err(err));
                        }
                    }
                    None => warn!("Could not decode extracted text"),
                },
                // PDF 32000-1 §9.4.3: `"` is `aw Tw ac Tc T* Tj`. Operands 0/1 only affect
                // rendering spacing; operand 2 is the string to show.
                "\"" => match current_encoding {
                    Some(encoding) => {
                        if !current_text.ends_with('\n') {
                            current_text.push('\n');
                        }
                        if let Some(string_operand) = operation.operands.get(2) {
                            let res = collect_text(&mut current_text, encoding, std::slice::from_ref(string_operand));
                            if let Err(err) = res {
                                collected_chunks_and_errs.push(Err(err));
                            }
                        }
                    }
                    None => warn!("Could not decode extracted text"),
                },
                // PDF 32000-1 §9.4.2: `T*` moves to the next line, approximated as `\n`
                // like the `ET` arm above.
                "T*" if !current_text.ends_with('\n') => current_text.push('\n'),
                "T*" => {}
                "ET" if !current_text.ends_with('\n') => current_text.push('\n'),
                "ET" => {}
                _ => {}
            }
        }
        if !current_text.is_empty() {
            collected_chunks_and_errs.push(Ok(current_text));
        }

        Ok(collected_chunks_and_errs)
    }

    pub fn replace_text(
        &mut self, page_number: u32, text: &str, other_text: &str, default_str: Option<&str>,
    ) -> Result<()> {
        let page = page_number.saturating_sub(1) as usize;
        let page_id = self
            .page_iter()
            .nth(page)
            .ok_or(Error::PageNumberNotFound(page_number))?;
        let encodings: BTreeMap<Vec<u8>, Encoding> = self
            .get_page_fonts(page_id)?
            .into_iter()
            .map(|(name, font)| font.get_font_encoding(self).map(|it| (name, it)))
            .collect::<Result<BTreeMap<Vec<u8>, Encoding>>>()?;
        let content_data = self.get_page_content(page_id);
        let mut content = Content::decode(&content_data)?;
        let mut current_encoding = None;
        for operation in &mut content.operations {
            match operation.operator.as_ref() {
                "Tf" => {
                    let current_font = operation
                        .operands
                        .first()
                        .ok_or_else(|| Error::Syntax("missing font operand".to_string()))?
                        .as_name()?;
                    current_encoding = encodings.get(current_font);
                }
                "Tj" | "TJ" => match current_encoding {
                    Some(encoding) => {
                        try_to_replace_encoded_text(operation, encoding, text, other_text, default_str.unwrap_or(""))?
                    }
                    None => {
                        warn!("Could not decode extracted text, some of the occurances might not be properly replaced")
                    }
                },
                _ => {}
            }
        }
        let modified_content = content.encode()?;
        self.change_page_content(page_id, modified_content)
    }

    pub fn replace_partial_text(
        &mut self, page_number: u32, search_text: &str, replacement_text: &str, default_char: Option<&str>,
    ) -> Result<usize> {
        let page = page_number.saturating_sub(1) as usize;
        let page_id = self
            .page_iter()
            .nth(page)
            .ok_or(Error::PageNumberNotFound(page_number))?;

        let encodings: BTreeMap<Vec<u8>, Encoding> = self
            .get_page_fonts(page_id)?
            .into_iter()
            .map(|(name, font)| font.get_font_encoding(self).map(|it| (name, it)))
            .collect::<Result<BTreeMap<Vec<u8>, Encoding>>>()?;

        let content_data = self.get_page_content(page_id);
        let mut content = Content::decode(&content_data)?;
        let mut current_encoding = None;
        let mut replacement_count = 0;

        for operation in &mut content.operations {
            match operation.operator.as_ref() {
                "Tf" => {
                    let current_font = operation
                        .operands
                        .first()
                        .ok_or_else(|| Error::Syntax("missing font operand".to_string()))?
                        .as_name()?;
                    current_encoding = encodings.get(current_font);
                }
                "Tj" | "TJ" => {
                    if let Some(encoding) = current_encoding {
                        replacement_count += replace_partial_in_operation(
                            operation,
                            encoding,
                            search_text,
                            replacement_text,
                            default_char.unwrap_or("?"),
                        )?;
                    } else {
                        warn!("No encoding found for text operation");
                    }
                }
                _ => {}
            }
        }

        if replacement_count > 0 {
            let modified_content = content.encode()?;
            self.change_page_content(page_id, modified_content)?;
        }

        Ok(replacement_count)
    }

    pub fn insert_image(
        &mut self, page_id: ObjectId, img_object: Stream, position: (f32, f32), size: (f32, f32),
    ) -> Result<()> {
        let img_id = self.add_object(img_object);
        let img_name = format!("X{}", img_id.0);

        self.add_xobject(page_id, img_name.as_bytes(), img_id)?;

        let mut content = self.get_and_decode_page_content(page_id)?;
        content.operations.push(Operation::new("q", vec![]));
        content.operations.push(Operation::new(
            "cm",
            vec![
                size.0.into(),
                0.into(),
                0.into(),
                size.1.into(),
                position.0.into(),
                position.1.into(),
            ],
        ));
        content
            .operations
            .push(Operation::new("Do", vec![Name(img_name.as_bytes().to_vec())]));
        content.operations.push(Operation::new("Q", vec![]));

        self.change_page_content(page_id, content.encode()?)
    }

    pub fn insert_form_object(&mut self, page_id: ObjectId, form_obj: Stream) -> Result<()> {
        let form_id = self.add_object(form_obj);
        let form_name = format!("X{}", form_id.0);

        let mut content = self.get_and_decode_page_content(page_id)?;
        content.operations.insert(0, Operation::new("q", vec![]));
        content.operations.push(Operation::new("Q", vec![]));
        content
            .operations
            .push(Operation::new("Do", vec![Name(form_name.as_bytes().to_vec())]));
        let modified_content = content.encode()?;
        self.add_xobject(page_id, form_name, form_id)?;

        self.change_page_content(page_id, modified_content)
    }
}
fn collect_text(text: &mut String, encoding: &Encoding, operands: &[Object]) -> Result<()> {
    for operand in operands.iter() {
        match operand {
            Object::String(bytes, _) => {
                encoding.write_to_string(bytes, text)?;
            }
            Object::Array(arr) => {
                collect_text(text, encoding, arr)?;
                text.push(' ');
            }
            Object::Integer(i) if *i < -100 => {
                text.push(' ');
            }
            _ => {}
        }
    }
    Ok(())
}

/// The characters of `s` starting at character index `start`, up to `len` of
/// them. Indices are counted in characters, not bytes, so a multi-byte
/// character is never split. Note that `len == 0` yields one character, not
/// none; this is long-standing behavior relied on by the sole caller.
pub fn substr(s: &str, start: usize, len: usize) -> &str {
    let mut indices = s.char_indices();

    for _ in 0..start {
        if indices.next().is_none() {
            return "";
        }
    }

    let Some((start_idx, _)) = indices.next() else {
        return "";
    };

    let end_idx = indices
        .nth(len.saturating_sub(1))
        .map(|(idx, _)| idx)
        .unwrap_or(s.len());

    &s[start_idx..end_idx]
}

/// Everything from character index `start` to the end of `s`.
pub fn substring(s: &str, start: usize) -> &str {
    s.char_indices().nth(start).map(|(idx, _)| &s[idx..]).unwrap_or("")
}

fn encode(encoding: &Encoding, txt: &str, default_str: &str) -> Vec<u8> {
    let mut chars = txt.chars();
    match (chars.next(), chars.next()) {
        // More than one character: encode each on its own, into one buffer.
        (Some(first), Some(second)) => {
            let mut result = Vec::with_capacity(txt.len());
            result.extend_from_slice(&encode_one(encoding, first, default_str));
            result.extend_from_slice(&encode_one(encoding, second, default_str));
            for c in chars {
                result.extend_from_slice(&encode_one(encoding, c, default_str));
            }
            result
        }
        // Exactly one character: encode it directly.
        (Some(c), None) => encode_one(encoding, c, default_str),
        // Empty: nothing to encode, but keep the caller's fallback contract.
        (None, _) => {
            let encoded = Document::encode_text(encoding, txt);
            if encoded.is_empty() {
                Document::encode_text(encoding, default_str)
            } else {
                encoded
            }
        }
    }
}

/// Encode a single character, falling back to `default_str` when the encoding
/// has no entry for it.
fn encode_one(encoding: &Encoding, c: char, default_str: &str) -> Vec<u8> {
    // A scratch buffer, so the common one-character case does not allocate a
    // `String` per character on the text-extraction path.
    let mut scratch = [0u8; 4];
    let encoded_bytes = Document::encode_text(encoding, c.encode_utf8(&mut scratch));

    if !encoded_bytes.is_empty() {
        encoded_bytes
    } else {
        Document::encode_text(encoding, default_str)
    }
}

fn try_to_replace_encoded_text(
    operation: &mut Operation, encoding: &Encoding, text_to_replace: &str, replacement: &str, default_str: &str,
) -> Result<()> {
    for operand in &mut operation.operands {
        match operand {
            Object::String(bytes, _) => {
                let decoded_text = Document::decode_text(encoding, bytes)?;
                if decoded_text == text_to_replace {
                    let encoded_bytes = encode(encoding, replacement, default_str);
                    *bytes = encoded_bytes;
                }
            }
            Object::Array(arr) => {
                let mut str_collected = String::new();
                collect_text(&mut str_collected, encoding, arr)?;
                if str_collected == text_to_replace {
                    // The `Object::String` count in a `TJ` array need not match the decoded
                    // character count, and kerning entries are interleaved. Least bad option:
                    // fill the first string slot with the whole replacement, empty the rest,
                    // and leave the kerning entries in place.
                    let encoded_replacement = encode(encoding, replacement, default_str);
                    let mut placed = false;
                    for item in arr.iter_mut() {
                        if let Object::String(bytes, _f) = item {
                            if placed {
                                *bytes = Vec::new();
                            } else {
                                bytes.clone_from(&encoded_replacement);
                                placed = true;
                            }
                        }
                    }
                }
            }
            _ => {}
        }
    }

    Ok(())
}

fn replace_partial_in_operation(
    operation: &mut Operation, encoding: &Encoding, search_text: &str, replacement_text: &str, default_char: &str,
) -> Result<usize> {
    let mut replacement_count = 0;

    for operand in &mut operation.operands {
        match operand {
            Object::String(bytes, _) => {
                let decoded_text = Document::decode_text(encoding, bytes)?;
                if decoded_text.contains(search_text) {
                    let new_text = decoded_text.replace(search_text, replacement_text);
                    let encoded_bytes = encode_with_fallback(encoding, &new_text, default_char);
                    *bytes = encoded_bytes;
                    replacement_count += decoded_text.matches(search_text).count();
                }
            }
            Object::Array(arr) => {
                replacement_count +=
                    replace_partial_in_array(arr, encoding, search_text, replacement_text, default_char)?;
            }
            _ => {}
        }
    }

    Ok(replacement_count)
}

fn replace_partial_in_array(
    arr: &mut [Object], encoding: &Encoding, search_text: &str, replacement_text: &str, default_char: &str,
) -> Result<usize> {
    let mut replacement_count = 0;

    for item in arr.iter_mut() {
        if let Object::String(bytes, _) = item {
            let decoded_text = Document::decode_text(encoding, bytes)?;
            if decoded_text.contains(search_text) {
                let new_text = decoded_text.replace(search_text, replacement_text);
                let encoded_bytes = encode_with_fallback(encoding, &new_text, default_char);
                *bytes = encoded_bytes;
                replacement_count += decoded_text.matches(search_text).count();
            }
        }
    }

    Ok(replacement_count)
}

fn encode_with_fallback(encoding: &Encoding, text: &str, default_char: &str) -> Vec<u8> {
    let encoded = Document::encode_text(encoding, text);
    if !encoded.is_empty() {
        return encoded;
    }

    encode(encoding, text, default_char)
}

/// Decode CrossReferenceStream
pub fn decode_xref_stream(stream: Stream) -> Result<(Xref, Dictionary)> {
    decode_xref_stream_with_limit(stream, None)
}

/// Decode a cross-reference stream, rejecting it if its decompressed content
/// would exceed `max_decompressed_size` bytes. `None` means no limit (the
/// behavior of [`decode_xref_stream`]). Cross-reference streams are decoded
/// early during loading, so this bounds the memory a `/XRef` stream can use.
pub fn decode_xref_stream_with_limit(
    mut stream: Stream, max_decompressed_size: Option<usize>,
) -> Result<(Xref, Dictionary)> {
    if stream.is_compressed() {
        match max_decompressed_size {
            Some(max) => stream.decompress_with_limit(max)?,
            None => stream.decompress()?,
        }
    } else if crate::StreamBudget::current().is_some() {
        // Move the unfiltered bytes into the decoder accounting without copying.
        stream.content = crate::stream_budget::decode(max_decompressed_size, |cap| {
            if let Some(limit) = cap
                && stream.content.len() > limit
            {
                return Err(crate::DecompressError::MemoryLimitExceeded { limit }.into());
            }
            Ok(std::mem::take(&mut stream.content))
        })?;
    }
    let mut dict = stream.dict;
    let mut reader = Cursor::new(stream.content);
    let size = dict
        .get(b"Size")
        .and_then(Object::as_i64)
        .map_err(|_| ParseError::InvalidXref)?;
    let mut xref = Xref::new(size as u32, XrefType::CrossReferenceStream);
    {
        let section_indice = dict
            .get(b"Index")
            .and_then(parse_integer_array)
            .unwrap_or_else(|_| vec![0, size]);
        let field_widths = dict
            .get(b"W")
            .and_then(parse_integer_array)
            .map_err(|_| ParseError::InvalidXref)?;

        // Each width is a byte count for one xref-stream field; 8 covers the full u64/i64 range.
        // Bound them before sizing a Vec, or a crafted stream requests up to 2^64-1 bytes per field.
        const MAX_XREF_FIELD_WIDTH: i64 = 8;
        if field_widths.len() < 3
            || field_widths[0].is_negative()
            || field_widths[1].is_negative()
            || field_widths[2].is_negative()
            || field_widths[0] > MAX_XREF_FIELD_WIDTH
            || field_widths[1] > MAX_XREF_FIELD_WIDTH
            || field_widths[2] > MAX_XREF_FIELD_WIDTH
        {
            return Err(ParseError::InvalidXref.into());
        }

        // Total bytes one entry consumes. With every width zero the loop below would spin on
        // attacker-controlled /Index counts forever, so reject it (also avoids a division by zero).
        let entry_width = (field_widths[0] + field_widths[1] + field_widths[2]) as usize;
        if entry_width == 0 {
            return Err(ParseError::InvalidXref.into());
        }

        let index_entries = section_indice
            .as_chunks::<2>()
            .0
            .iter()
            .try_fold(0_usize, |total, section| {
                let count = usize::try_from(section[1]).map_err(|_| ParseError::InvalidXref)?;
                total.checked_add(count).ok_or(ParseError::InvalidXref)
            })?;
        // Validate the total before inserting anything so multiple plausible /Index sections
        // cannot overrun the body. Records narrower than three bytes are rejected to limit
        // amplification; lopdf's writer emits /W [1 4 2], so its own output is unaffected.
        const MIN_ENTRY_WIDTH: usize = 3;
        if index_entries > reader.get_ref().len() / entry_width.max(MIN_ENTRY_WIDTH) {
            return Err(ParseError::InvalidXref.into());
        }

        let mut bytes1 = vec![0_u8; field_widths[0] as usize];
        let mut bytes2 = vec![0_u8; field_widths[1] as usize];
        let mut bytes3 = vec![0_u8; field_widths[2] as usize];

        for section in section_indice.as_chunks::<2>().0 {
            let start = section[0];
            let count = section[1];
            let mut skipped = 0usize;

            for j in 0..count {
                let entry_type = if !bytes1.is_empty() {
                    read_big_endian_integer(&mut reader, bytes1.as_mut_slice())?
                } else {
                    1
                };
                let entry = match entry_type {
                    0 => {
                        // free object
                        read_big_endian_integer(&mut reader, bytes2.as_mut_slice())?;
                        read_big_endian_integer(&mut reader, bytes3.as_mut_slice())?;
                        None
                    }
                    1 => {
                        // normal object
                        let offset = read_big_endian_integer(&mut reader, bytes2.as_mut_slice())?;
                        let generation = if !bytes3.is_empty() {
                            read_big_endian_integer(&mut reader, bytes3.as_mut_slice())?
                        } else {
                            0
                        } as u16;
                        Some(XrefEntry::Normal { offset, generation })
                    }
                    2 => {
                        // compressed object
                        let container = read_big_endian_integer(&mut reader, bytes2.as_mut_slice())?;
                        let index = read_big_endian_integer(&mut reader, bytes3.as_mut_slice())? as u16;
                        Some(XrefEntry::Compressed { container, index })
                    }
                    _ => {
                        // unknown types are null references, but their fields still take up space
                        read_big_endian_integer(&mut reader, bytes2.as_mut_slice())?;
                        read_big_endian_integer(&mut reader, bytes3.as_mut_slice())?;
                        None
                    }
                };
                // `start` is untrusted, as in a cross-reference table: `start + j` can overflow,
                // and a `start` outside the u32 range would truncate into a valid-looking number.
                if let Some(entry) = entry {
                    match start.checked_add(j).and_then(|id| u32::try_from(id).ok()) {
                        Some(id) => xref.insert(id, entry),
                        None => skipped += 1,
                    }
                }
            }
            if skipped > 0 {
                warn!(
                    "cross-reference stream section starting at {start} has {skipped} entr{} with an object number outside u32; skipping",
                    if skipped == 1 { "y" } else { "ies" }
                );
            }
        }
    }
    dict.remove(b"Length");
    dict.remove(b"W");
    dict.remove(b"Index");
    Ok((xref, dict))
}

fn read_big_endian_integer(reader: &mut Cursor<Vec<u8>>, buffer: &mut [u8]) -> Result<u32> {
    reader.read_exact(buffer)?;
    let mut value = 0;
    for &mut byte in buffer {
        value = (value << 8) + u32::from(byte);
    }
    Ok(value)
}

fn parse_integer_array(array: &Object) -> Result<Vec<i64>> {
    let array = array.as_array()?;
    let mut out = Vec::with_capacity(array.len());

    for n in array {
        out.push(n.as_i64()?);
    }

    Ok(out)
}
