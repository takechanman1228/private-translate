use crate::{
    Error, Object, Result, StringFormat,
    encodings::{self, bytes_to_string},
};

/// Creates a text string.
/// If the input only contains ASCII characters, the string is encoded
/// in PDFDocEncoding, otherwise in UTF-16BE.
pub fn text_string(text: &str) -> Object {
    if text.is_ascii() {
        return Object::String(text.into(), StringFormat::Literal);
    }
    let mut string = Vec::new();
    encodings::encode_utf16_be(text, &mut string);
    Object::String(string, StringFormat::Hexadecimal)
}

/// Decodes a text string.
/// Depending on the BOM at the start of the string, a different encoding is chosen.
/// All encodings specified in PDF2.0 are supported (PDFDocEncoding, UTF-16BE,
/// and UTF-8).
pub fn decode_text_string(obj: &Object) -> Result<String> {
    let s = obj.as_str()?;
    if s.starts_with(b"\xFE\xFF") {
        // Detected UTF-16BE BOM. An odd byte left at the end is not a whole code unit and is ignored.
        String::from_utf16(
            &s[2..]
                .as_chunks::<2>()
                .0
                .iter()
                .map(|&c| u16::from_be_bytes(c))
                .collect::<Vec<u16>>(),
        )
        .map_err(|_| Error::TextStringDecode)
    } else if s.starts_with(b"\xEF\xBB\xBF") {
        // Detected UTF-8 BOM
        String::from_utf8(s[3..].to_vec()).map_err(|_| Error::TextStringDecode)
    } else {
        // If neither BOM is detected, PDFDocEncoding is used
        let mut out = String::new();
        bytes_to_string(&encodings::PDF_DOC_ENCODING, s, &mut out)?;
        Ok(out)
    }
}
