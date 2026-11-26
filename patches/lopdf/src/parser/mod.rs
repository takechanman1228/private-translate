use super::{Dictionary, Object, ObjectId, Reader, Stream, StringFormat};
use crate::Error;
use crate::content::*;
use crate::error;
use crate::xref::*;
use std::collections::HashSet;
use std::str::{self, FromStr};

use nom::branch::alt;
use nom::bytes::complete::{tag, take, take_while, take_while_m_n, take_while1};
use nom::character::complete::multispace1;
use nom::character::complete::{digit0, digit1, one_of};
use nom::character::complete::{space0, space1};
use nom::combinator::cut;
use nom::combinator::{map, map_opt, map_res, opt, verify};
use nom::error::{ErrorKind, ParseError};
use nom::multi::{fold_many0, fold_many1, many0, many0_count};
use nom::sequence::{delimited, pair, preceded, separated_pair, terminated};
use nom::{AsChar, IResult, Input, Parser};

pub(crate) mod cmap_parser;

pub(crate) type ParserInput<'a> = &'a [u8];
// Change this to something else that implements ParseError to get a
// different error type out of nom.
pub(crate) type NomError<'a> = nom::error::Error<ParserInput<'a>>;

pub(crate) type NomResult<'a, O, E = NomError<'a>> = IResult<ParserInput<'a>, O, E>;

#[inline]
fn strip_nom<O>(r: NomResult<O>) -> Option<O> {
    r.ok().map(|(_, o)| o)
}

#[inline]
fn convert_result<O, E>(result: Result<O, E>, input: ParserInput, error_kind: ErrorKind) -> NomResult<O> {
    result.map(|o| (input, o)).map_err(|_| {
        // this is a unit bind if NomError = ()
        let err: NomError = nom::error::Error::from_error_kind(input, error_kind);
        nom::Err::Error(err)
    })
}

#[inline]
fn offset_stream(object: &mut Object, offset: usize) {
    if let Object::Stream(stream) = object {
        stream.start_position = stream.start_position.and_then(|sp| sp.checked_add(offset));
    }
}

pub(crate) fn eol(input: ParserInput) -> NomResult<ParserInput> {
    alt((tag(&b"\r\n"[..]), tag(&b"\n"[..]), tag(&b"\r"[..]))).parse(input)
}

pub(crate) fn comment(input: ParserInput) -> NomResult<()> {
    map((tag(&b"%"[..]), take_while(|c: u8| !b"\r\n".contains(&c)), eol), |_| ()).parse(input)
}

#[inline]
fn is_whitespace(c: u8) -> bool {
    b" \t\n\r\0\x0C".contains(&c)
}

#[inline]
fn is_delimiter(c: u8) -> bool {
    b"()<>[]{}/%".contains(&c)
}

#[inline]
fn is_regular(c: u8) -> bool {
    !is_whitespace(c) && !is_delimiter(c)
}

#[inline]
fn is_direct_literal_string(c: u8) -> bool {
    !b"()\\\r\n".contains(&c)
}

fn white_space(input: ParserInput) -> NomResult<()> {
    map(take_while(is_whitespace), |_| ()).parse(input)
}

/// White space and comments, as many as there are.
fn space(mut input: ParserInput) -> NomResult<()> {
    loop {
        let blanks = input.iter().take_while(|&&c| is_whitespace(c)).count();
        if blanks > 0 {
            input = &input[blanks..];
        } else if input.first() == Some(&b'%')
            && let Ok((rest, ())) = comment(input)
        {
            input = rest;
        } else {
            return Ok((input, ()));
        }
    }
}

/// The value of a run of ASCII digits, or `None` if it exceeds `u64`.
fn digits_value(digits: &[u8]) -> Option<u64> {
    digits.iter().try_fold(0u64, |value, &digit| {
        value.checked_mul(10)?.checked_add(u64::from(digit - b'0'))
    })
}

/// An optional `+` or `-` sign, and whether it was `-`.
fn sign(input: ParserInput) -> NomResult<bool> {
    match input.split_first() {
        Some((b'+', rest)) => Ok((rest, false)),
        Some((b'-', rest)) => Ok((rest, true)),
        _ => Ok((input, false)),
    }
}

fn integer(input: ParserInput) -> NomResult<i64> {
    let (i, (negative, digits)) = pair(sign, digit1).parse(input)?;

    let value = digits_value(digits).and_then(|magnitude| {
        if negative {
            0i64.checked_sub_unsigned(magnitude)
        } else {
            i64::try_from(magnitude).ok()
        }
    });
    convert_result(value.ok_or(()), i, ErrorKind::Digit)
}

fn real(input: ParserInput) -> NomResult<f32> {
    let (i, _) = pair(
        sign,
        alt((
            map((digit1, tag(&b"."[..]), digit0), |_| ()),
            map(pair(tag(&b"."[..]), digit1), |_| ()),
        )),
    )
    .parse(input)?;
    // Some producers write a minus sign inside the fraction, as in `0.00-4142502`. Like
    // pdf.js and Poppler, ignore it rather than splitting the number in two.
    let (i, fraction) = take_while(|c: u8| c == b'-' || c.is_ascii_digit()).parse(i)?;

    let float_input = &input[..input.len() - i.len()];
    let value = if fraction.contains(&b'-') {
        let (sign, rest) = float_input.split_at(1);
        let mut digits = sign.to_vec();
        digits.extend(rest.iter().filter(|&&c| c != b'-'));
        f32::from_str(str::from_utf8(&digits).unwrap())
    } else {
        f32::from_str(str::from_utf8(float_input).unwrap())
    };
    convert_result(value, i, ErrorKind::Digit)
}

/// The value of an ASCII hexadecimal digit.
#[inline]
fn hex_value(c: u8) -> Option<u8> {
    (c as char).to_digit(16).map(|value| value as u8)
}

pub(crate) fn hex_char(input: ParserInput) -> NomResult<u8> {
    map_opt(take(2usize), |h: ParserInput| {
        Some((hex_value(h[0])? << 4) | hex_value(h[1])?)
    })
    .parse(input)
}

fn oct_char(input: ParserInput) -> NomResult<u8> {
    map_res(
        take_while_m_n(1, 3, AsChar::is_oct_digit),
        // Spec requires us to ignore any overflow.
        |x: ParserInput| u16::from_str_radix(str::from_utf8(x).unwrap(), 8).map(|o| o as u8),
    )
    .parse(input)
}

pub(crate) fn name(input: ParserInput) -> NomResult<Vec<u8>> {
    let (mut input, _) = tag(&b"/"[..]).parse(input)?;
    let mut bytes = Vec::new();
    loop {
        // Copy regular characters a run at a time, up to the next `#` or the end of the name.
        let run = input
            .iter()
            .position(|&c| c == b'#' || !is_regular(c))
            .unwrap_or(input.len());
        bytes.extend_from_slice(&input[..run]);
        input = &input[run..];
        // `#` and two hexadecimal digits stand for one byte; any other `#` ends the name.
        match input.split_first() {
            Some((b'#', escape)) => match hex_char(escape) {
                Ok((rest, byte)) => {
                    bytes.push(byte);
                    input = rest;
                }
                Err(_) => return Ok((input, bytes)),
            },
            _ => return Ok((input, bytes)),
        }
    }
}

fn escape_sequence(input: ParserInput) -> NomResult<Option<u8>> {
    preceded(
        tag(&b"\\"[..]),
        alt((
            map(oct_char, Some),
            map(eol, |_| None),
            map(tag(&b"n"[..]), |_| Some(b'\n')),
            map(tag(&b"r"[..]), |_| Some(b'\r')),
            map(tag(&b"t"[..]), |_| Some(b'\t')),
            map(tag(&b"b"[..]), |_| Some(b'\x08')),
            map(tag(&b"f"[..]), |_| Some(b'\x0C')),
            map(take(1usize), |c: ParserInput| Some(c[0])),
        )),
    )
    .parse(input)
}

enum InnerLiteralString<'a> {
    Direct(ParserInput<'a>),
    Escape(Option<u8>),
    Eol,
    Nested(Vec<u8>),
}

impl InnerLiteralString<'_> {
    fn push(&self, output: &mut Vec<u8>) {
        match self {
            InnerLiteralString::Direct(s) => output.extend_from_slice(s),
            InnerLiteralString::Eol => output.push(b'\n'),
            InnerLiteralString::Escape(e) => output.extend(e),
            InnerLiteralString::Nested(n) => output.extend_from_slice(n),
        }
    }
}

fn inner_literal_string(depth: usize) -> impl Fn(ParserInput) -> NomResult<Vec<u8>> {
    move |input| {
        fold_many0(
            alt((
                map(take_while1(is_direct_literal_string), InnerLiteralString::Direct),
                map(escape_sequence, InnerLiteralString::Escape),
                map(eol, |_| InnerLiteralString::Eol),
                map(nested_literal_string(depth), InnerLiteralString::Nested),
            )),
            Vec::new,
            |mut out: Vec<u8>, value| {
                value.push(&mut out);
                out
            },
        )
        .parse(input)
    }
}

fn nested_literal_string(depth: usize) -> impl Fn(ParserInput) -> NomResult<Vec<u8>> {
    move |input| {
        if depth == 0 {
            map(verify(tag(&b"too deep"[..]), |_: &[u8]| false), |_| vec![]).parse(input)
        } else {
            map(
                delimited(tag(&b"("[..]), inner_literal_string(depth - 1), tag(&b")"[..])),
                |mut content| {
                    content.insert(0, b'(');
                    content.push(b')');
                    content
                },
            )
            .parse(input)
        }
    }
}

fn literal_string(input: ParserInput) -> NomResult<Vec<u8>> {
    delimited(
        tag(&b"("[..]),
        inner_literal_string(crate::reader::MAX_BRACKET),
        tag(&b")"[..]),
    )
    .parse(input)
}

#[inline]
fn hex_digit(input: ParserInput) -> NomResult<u8> {
    map_opt(take(1usize), |c: ParserInput| hex_value(c[0])).parse(input)
}

fn hexadecimal_string(input: ParserInput) -> NomResult<Object> {
    map(
        delimited(
            tag(&b"<"[..]),
            terminated(
                fold_many0(
                    preceded(white_space, hex_digit),
                    || -> (Vec<u8>, bool) { (Vec::new(), false) },
                    |state, c| match state {
                        (mut out, false) => {
                            out.push(c << 4);
                            (out, true)
                        }
                        (mut out, true) => {
                            *out.last_mut().unwrap() |= c;
                            (out, false)
                        }
                    },
                ),
                white_space,
            ),
            tag(&b">"[..]),
        ),
        |(bytes, _)| Object::String(bytes, StringFormat::Hexadecimal),
    )
    .parse(input)
}

fn boolean(input: ParserInput) -> NomResult<Object> {
    alt((
        map(tag(&b"true"[..]), |_| Object::Boolean(true)),
        map(tag(&b"false"[..]), |_| Object::Boolean(false)),
    ))
    .parse(input)
}

fn null(input: ParserInput) -> NomResult<Object> {
    map(tag(&b"null"[..]), |_| Object::Null).parse(input)
}

fn array(depth: usize) -> impl Fn(ParserInput) -> NomResult<Vec<Object>> {
    move |input| {
        delimited(
            pair(tag(&b"["[..]), space),
            many0(_direct_object(depth)),
            tag(&b"]"[..]),
        )
        .parse(input)
    }
}

pub(crate) fn dictionary(input: ParserInput) -> NomResult<Dictionary> {
    _dictionary(crate::reader::MAX_NESTING_DEPTH)(input)
}

fn _dictionary(depth: usize) -> impl Fn(ParserInput) -> NomResult<Dictionary> {
    move |input| delimited(pair(tag(&b"<<"[..]), space), inner_dictionary(depth), tag(&b">>"[..])).parse(input)
}

fn inner_dictionary(depth: usize) -> impl Fn(ParserInput) -> NomResult<Dictionary> {
    move |input| {
        fold_many0(
            pair(terminated(name, space), _direct_object(depth)),
            Dictionary::new,
            |mut dict, (key, value)| {
                dict.set(key, value);
                dict
            },
        )
        .parse(input)
    }
}

pub(crate) fn dict_dup(input: ParserInput) -> NomResult<Dictionary> {
    delimited(
        (
            digit1,
            space1,
            tag(&b"dict"[..]),
            space1,
            tag(&b"dup"[..]),
            space1,
            tag(&b"begin"[..]),
            multispace1,
        ),
        fold_many0(
            terminated(
                pair(
                    terminated(name, space),
                    _direct_object(crate::reader::MAX_NESTING_DEPTH),
                ),
                pair(tag(&b"def"[..]), multispace1),
            ),
            Dictionary::new,
            |mut dict, (key, value)| {
                dict.set(key, value);
                dict
            },
        ),
        tag(&b"end"[..]),
    )
    .parse(input)
}

/// Recover the sole EOL-framed `endstream` immediately followed by `endobj`.
/// The caller must pass only bytes up to the current indirect-object boundary
/// so the scan cannot cross into a neighboring object.
fn recover_stream_length(input: ParserInput) -> Option<(ParserInput, ParserInput)> {
    const ENDSTREAM: &[u8] = b"endstream";
    let mut recovered = None;

    for (position, candidate) in input.windows(ENDSTREAM.len()).enumerate() {
        if candidate != ENDSTREAM {
            continue;
        }

        let data_end = if input[..position].ends_with(b"\r\n") {
            position - 2
        } else if input[..position].ends_with(b"\n") || input[..position].ends_with(b"\r") {
            position - 1
        } else {
            continue;
        };
        let after_endstream = &input[position + ENDSTREAM.len()..];
        let Ok((after_endobj, _)) = preceded(space, tag(&b"endobj"[..])).parse(after_endstream) else {
            continue;
        };
        if after_endobj.first().is_some_and(|&byte| !is_whitespace(byte)) {
            continue;
        }
        if recovered.is_some() {
            return None;
        }
        recovered = Some((after_endstream, &input[..data_end]));
    }

    recovered
}

/// The `stream` keyword and its end-of-line marker, which turn a dictionary into
/// the dictionary of a stream.
fn stream_keyword(input: ParserInput) -> NomResult<()> {
    map((space, tag(&b"stream"[..]), space0, eol), |_| ()).parse(input)
}

/// A stream whose dictionary `dict` starts `input`, with its data starting at `i`.
fn stream<'a>(
    input: ParserInput<'a>, i: ParserInput<'a>, dict: Dictionary, reader: &Reader,
    already_seen: &mut HashSet<ObjectId>, recover_length: bool, recovery_bound: Option<usize>,
) -> NomResult<'a, Object> {
    if let Ok(length) = dict.get(b"Length").and_then(|value| {
        if let Ok(id) = value.as_reference() {
            reader.get_object(id, already_seen).and_then(|value| value.as_i64())
        } else {
            value.as_i64()
        }
    }) {
        if length < 0 {
            // artificial error kind is created to allow descriptive nom errors
            return Err(nom::Err::Failure(NomError::from_error_kind(i, ErrorKind::LengthValue)));
        }
        let Ok(length) = usize::try_from(length) else {
            return Err(nom::Err::Failure(NomError::from_error_kind(i, ErrorKind::LengthValue)));
        };
        match terminated(take(length), pair(opt(eol), tag(&b"endstream"[..]))).parse(i) {
            Ok((remaining, data)) => Ok((remaining, Object::Stream(Stream::new(dict, data.to_vec())))),
            Err(_) if recover_length && !reader.strict => {
                // The scan must not cross into a neighbouring object, so it stops at the
                // xref-derived bound: the first `i.len() - bound` bytes of `i`.
                let scan_end = recovery_bound.map_or(i.len(), |bound| i.len().saturating_sub(bound));
                let Some((remaining, data)) = recover_stream_length(&i[..scan_end]) else {
                    return Err(nom::Err::Failure(NomError::from_error_kind(i, ErrorKind::LengthValue)));
                };
                log::warn!(
                    "Stream Length is {length}, but the unambiguous object boundary gives {} bytes; using the recovered length.",
                    data.len()
                );
                Ok((remaining, Object::Stream(Stream::new(dict, data.to_vec()))))
            }
            Err(_) => Err(nom::Err::Failure(NomError::from_error_kind(i, ErrorKind::LengthValue))),
        }
    } else {
        // Return position relative to the start of the stream dictionary.
        Ok((i, Object::Stream(Stream::with_position(dict, input.len() - i.len()))))
    }
}

fn unsigned_int<I: TryFrom<u64>>(input: ParserInput) -> NomResult<I> {
    map_opt(digit1, |digits: ParserInput| {
        digits_value(digits).and_then(|value| I::try_from(value).ok())
    })
    .parse(input)
}

fn object_id(input: ParserInput) -> NomResult<ObjectId> {
    pair(terminated(unsigned_int, space), terminated(unsigned_int, space)).parse(input)
}

fn reference(input: ParserInput) -> NomResult<Object> {
    map(terminated(object_id, tag(&b"R"[..])), Object::Reference).parse(input)
}

fn _direct_objects(depth: usize) -> impl Fn(ParserInput) -> NomResult<Object> {
    // The first byte tells the kinds of object apart, so it selects the parsers to try.
    // An integer also matches the start of a reference or a real, so it is tried last.
    move |input| match input.first() {
        Some(b'n') => null(input),
        Some(b't' | b'f') => boolean(input),
        Some(b'0'..=b'9') => alt((reference, map(real, Object::Real), map(integer, Object::Integer))).parse(input),
        Some(b'+' | b'-' | b'.') => alt((map(real, Object::Real), map(integer, Object::Integer))).parse(input),
        Some(b'/') => map(name, Object::Name).parse(input),
        Some(b'(') => map(literal_string, Object::string_literal).parse(input),
        Some(b'<') => alt((hexadecimal_string, map(_dictionary(depth), Object::Dictionary))).parse(input),
        Some(b'[') => map(array(depth), Object::Array).parse(input),
        _ => Err(nom::Err::Error(NomError::from_error_kind(input, ErrorKind::Alt))),
    }
}

fn _direct_object(depth: usize) -> impl Fn(ParserInput) -> NomResult<Object> {
    move |input| {
        if depth == 0 {
            return Err(nom::Err::Failure(NomError::from_error_kind(input, ErrorKind::TooLarge)));
        }
        terminated(_direct_objects(depth - 1), space).parse(input)
    }
}

pub fn direct_object(input: ParserInput) -> Option<Object> {
    strip_nom(_direct_object(crate::reader::MAX_NESTING_DEPTH)(input))
}

fn object<'a>(
    input: ParserInput<'a>, reader: &Reader, already_seen: &mut HashSet<ObjectId>, recover_stream_length: bool,
    recovery_bound: Option<usize>,
) -> NomResult<'a, Object> {
    let (i, object) = if input.starts_with(b"<<") {
        // A stream starts with its dictionary: parse the dictionary once, and let the
        // keyword after it decide whether the object is a stream.
        let (i, dict) = dictionary(input)?;
        match stream_keyword(i) {
            Ok((data, ())) => stream(
                input,
                data,
                dict,
                reader,
                already_seen,
                recover_stream_length,
                recovery_bound,
            )?,
            Err(_) => (i, Object::Dictionary(dict)),
        }
    } else {
        _direct_objects(crate::reader::MAX_NESTING_DEPTH)(input)?
    };
    let (i, ()) = space(i)?;
    Ok((i, object))
}

pub fn indirect_object(
    input: ParserInput, offset: usize, expected_id: Option<ObjectId>, reader: &Reader,
    already_seen: &mut HashSet<ObjectId>, recovery_bound: Option<usize>,
) -> crate::Result<(ObjectId, Object)> {
    // Every downstream slice is a suffix of `input`, so express the absolute
    // end offset as the maximum length a suffix may have.
    let recovery_bound = recovery_bound.map(|end| input.len().saturating_sub(end));
    let (id, mut object) = _indirect_object(
        input.take_from(offset),
        offset,
        expected_id,
        reader,
        already_seen,
        true,
        recovery_bound,
    )?;

    offset_stream(&mut object, offset);

    Ok((id, object))
}

fn _indirect_object<'a>(
    input: ParserInput<'a>, offset: usize, expected_id: Option<ObjectId>, reader: &Reader,
    already_seen: &mut HashSet<ObjectId>, recover_stream_length: bool, recovery_bound: Option<usize>,
) -> crate::Result<(ObjectId, Object)> {
    let (i, (_, object_id)) = terminated((space, object_id), pair(tag(&b"obj"[..]), space))
        .parse(input)
        .map_err(|_| Error::IndirectObject { offset })?;
    if let Some(expected_id) = expected_id
        && object_id != expected_id
    {
        return Err(crate::error::Error::ObjectIdMismatch);
    }

    let object_offset = input.len() - i.len();
    let (_, mut object) = terminated(
        |i: ParserInput<'a>| object(i, reader, already_seen, recover_stream_length, recovery_bound),
        (space, opt(tag(&b"endobj"[..])), space),
    )
    .parse(i)
    .map_err(|_| Error::IndirectObject { offset })?;

    offset_stream(&mut object, object_offset);

    Ok((object_id, object))
}

pub fn header(input: ParserInput, strict: bool) -> Option<String> {
    // Parse the version digits separately from trailing bytes: some generators put binary
    // markers on the header line. Strict mode rejects them, lenient mode skips them.
    let (_, (version_raw, trailing)) = delimited(
        tag(&b"%PDF-"[..]),
        pair(
            take_while(|c: u8| c.is_ascii_digit() || c == b'.'),
            take_while(|c: u8| !b"\r\n".contains(&c)),
        ),
        pair(eol, many0_count(comment)),
    )
    .parse(input)
    .ok()?;

    if strict && !trailing.is_empty() {
        return None;
    }

    let version = str::from_utf8(version_raw).ok()?.to_string();
    Some(version)
}

pub fn binary_mark(input: ParserInput) -> Option<Vec<u8>> {
    strip_nom(
        map_res(
            delimited(
                tag(&b"%"[..]),
                take_while(|c: u8| !b"\r\n".contains(&c)),
                pair(eol, many0_count(comment)),
            ),
            |v: ParserInput| Ok::<Vec<u8>, ()>(v.to_vec()),
        )
        .parse(input),
    )
}

/// Decode CrossReferenceTable
fn xref(input: ParserInput, strict: bool) -> NomResult<Xref> {
    // ISO 32000-1 s7.5.4 requires 20-byte entries ending in SP CR, SP LF or CR LF, but many
    // generators emit 19-byte entries ending in a bare LF, which qpdf, pikepdf, PDFium and
    // PDF.js accept. The conforming forms come first in the `alt` so a bare CR never matches
    // the CR of a CR LF and strands its LF.
    let xref_eol = move |i| {
        let conforming = alt((tag(&b" \r"[..]), tag(&b" \n"[..]), tag(&b"\r\n"[..])));
        if strict {
            map(conforming, |_| ()).parse(i)
        } else {
            map(alt((conforming, tag(&b"\n"[..]), tag(&b"\r"[..]))), |_| ()).parse(i)
        }
    };
    let xref_entry = pair(
        separated_pair(unsigned_int, tag(&b" "[..]), unsigned_int::<u32>),
        delimited(tag(&b" "[..]), map(one_of("nf"), |k| k == 'n'), xref_eol),
    );

    let xref_section = pair(
        separated_pair(unsigned_int::<usize>, tag(&b" "[..]), unsigned_int::<u32>),
        preceded(pair(opt(tag(&b" "[..])), eol), many0(xref_entry)),
    );

    delimited(
        pair(tag(&b"xref"[..]), preceded(opt(tag(&b" "[..])), eol)),
        fold_many1(
            xref_section,
            || -> Xref { Xref::new(0, XrefType::CrossReferenceTable) },
            |mut xref, ((start, _count), entries)| {
                let mut skipped = 0usize;
                for (index, ((offset, generation), is_normal)) in entries.into_iter().enumerate() {
                    if is_normal && let Ok(generation) = generation.try_into() {
                        // `start` is untrusted: `start + index` can overflow, and a `start`
                        // above u32::MAX would truncate into a valid-looking object number.
                        match start.checked_add(index).and_then(|id| u32::try_from(id).ok()) {
                            Some(id) => xref.insert(id, XrefEntry::Normal { offset, generation }),
                            None => skipped += 1,
                        }
                    }
                }
                if skipped > 0 {
                    log::warn!(
                        "cross-reference subsection starting at {start} has {skipped} entr{} with an object number beyond u32; skipping",
                        if skipped == 1 { "y" } else { "ies" }
                    );
                }
                xref
            },
        ),
        space,
    )
    .parse(input)
}

fn trailer(input: ParserInput) -> NomResult<Dictionary> {
    delimited(pair(tag(&b"trailer"[..]), space), dictionary, space).parse(input)
}

pub fn xref_and_trailer(input: ParserInput, reader: &Reader) -> crate::Result<(Xref, Dictionary)> {
    let xref_trailer = map(pair(|i| xref(i, reader.strict), trailer), |(mut xref, trailer)| {
        xref.size = trailer
            .get(b"Size")
            .and_then(Object::as_i64)
            .map_err(|_| error::ParseError::InvalidTrailer)? as u32;
        Ok((xref, trailer))
    });
    alt((
        xref_trailer,
        (|input| {
            _indirect_object(input, 0, None, reader, &mut HashSet::new(), false, None)
                .map(|(_, obj)| {
                    let res = match obj {
                        Object::Stream(stream) => decode_xref_stream_with_limit(stream, reader.max_decompressed_size),
                        _ => Err(crate::error::ParseError::InvalidXref.into()),
                    };
                    (input, res)
                })
                .map_err(|_| {
                    // artificial error kind is created to allow descriptive nom errors
                    nom::Err::Error(NomError::from_error_kind(input, ErrorKind::Fail))
                })
        }),
    ))
    .parse(input)
    .map(|(_, o)| o)
    .map_err(|_| error::ParseError::InvalidTrailer)?
}

pub fn xref_start(input: ParserInput) -> Option<i64> {
    strip_nom(
        delimited(
            pair(tag(&b"startxref"[..]), preceded(opt(tag(&b" "[..])), eol)),
            trim_spaces(integer),
            (eol, tag(&b"%%EOF"[..]), space),
        )
        .parse(input),
    )
}

fn trim_spaces<'a, O>(
    p: impl Parser<ParserInput<'a>, Output = O, Error = NomError<'a>>,
) -> impl Parser<ParserInput<'a>, Output = O, Error = NomError<'a>> {
    delimited(many0(tag(" ")), p, many0(tag(" ")))
}

// The following code create parser to parse content stream.

fn content_space(input: ParserInput) -> NomResult<()> {
    map(take_while(is_whitespace), |_| ()).parse(input)
}

fn operator(input: ParserInput) -> NomResult<String> {
    map_res(
        // Digits may follow the first character, as in the Type 3 glyph operators d0 and d1.
        verify(
            take_while1(|c: u8| c.is_ascii_alphanumeric() || b"*'\"".contains(&c)),
            |op: ParserInput| !op[0].is_ascii_digit(),
        ),
        |op: ParserInput| str::from_utf8(op).map(Into::into),
    )
    .parse(input)
}

fn operand(input: ParserInput) -> NomResult<Object> {
    terminated(
        alt((
            null,
            boolean,
            map(real, Object::Real),
            map(integer, Object::Integer),
            map(name, Object::Name),
            map(literal_string, Object::string_literal),
            hexadecimal_string,
            map(array(crate::reader::MAX_NESTING_DEPTH), Object::Array),
            map(dictionary, Object::Dictionary),
        )),
        content_space,
    )
    .parse(input)
}

fn operation(input: ParserInput) -> NomResult<Operation> {
    map(
        preceded(
            // A comment consumes its end-of-line marker, so also skip any
            // white space that follows it (e.g. an indented next line).
            many0(terminated(comment, content_space)),
            alt((inline_image, terminated(pair(many0(operand), operator), content_space))),
        ),
        |(operands, operator)| Operation { operator, operands },
    )
    .parse(input)
}

fn inline_image(input: ParserInput) -> NomResult<(Vec<Object>, String)> {
    preceded(pair(tag(&b"BI"[..]), content_space), cut(inline_image_impl)).parse(input)
}

/// The single white-space character between `ID` and the image data (ISO 32000-2, 8.9.7),
/// with a CR LF pair read as one end-of-line. White space after it is image data: a gray
/// level of 32 is a space byte.
fn inline_image_data_separator(input: ParserInput) -> NomResult<()> {
    map(opt(alt((eol, take_while_m_n(1, 1, is_whitespace)))), |_| ()).parse(input)
}

fn inline_image_impl(input: ParserInput) -> NomResult<(Vec<Object>, String)> {
    let (input, stream_dict) = inner_dictionary(crate::reader::MAX_NESTING_DEPTH).parse(input)?;
    let (input, _) = pair(tag(&b"ID"[..]), inline_image_data_separator).parse(input)?;
    match image_data_stream(input, stream_dict) {
        Ok((input, stream)) => {
            let (input, _) = (content_space, tag(&b"EI"[..]), content_space).parse(input)?;
            Ok((input, (vec![Object::Stream(stream)], String::from("BI"))))
        }
        Err(e) => {
            // Skip to EI marker so the rest of the content stream can still be parsed.
            log::warn!("Skipping unparseable inline image: {e}");
            let bytes = input;
            // EI must appear after whitespace to distinguish from data bytes.
            let ei_pos = bytes
                .windows(4)
                .position(|w| {
                    (w[0] == b' ' || w[0] == b'\n' || w[0] == b'\r')
                        && w[1] == b'E'
                        && w[2] == b'I'
                        && (w[3] == b' ' || w[3] == b'\n' || w[3] == b'\r')
                })
                .ok_or_else(|| {
                    let err: NomError = nom::error::Error::from_error_kind(input, ErrorKind::Fail);
                    nom::Err::Failure(err)
                })?;
            let (input, _) = take(ei_pos + 3).parse(input).map_err(|_: nom::Err<()>| {
                let err: NomError = nom::error::Error::from_error_kind(input, ErrorKind::Fail);
                nom::Err::Failure(err)
            })?;
            let (input, _) = content_space(input)?;
            Ok((input, (vec![], String::from("BI"))))
        }
    }
}

fn image_data_stream(input: ParserInput, stream_dict: Dictionary) -> crate::Result<(ParserInput, Stream)> {
    let get_abbr = |key_abbr: &[u8], key: &[u8]| stream_dict.get(key_abbr).or_else(|_| stream_dict.get(key));
    let invalid_size = || Error::InvalidInlineImage(String::from("image size out of range"));
    let get_size = |key_abbr: &[u8], key: &[u8]| -> crate::Result<usize> {
        usize::try_from(get_abbr(key_abbr, key)?.as_i64()?).map_err(|_| invalid_size())
    };
    let width = get_size(b"W", b"Width")?;
    let height = get_size(b"H", b"Height")?;
    let bpc = get_size(b"BPC", b"BitsPerComponent")?;
    let im = get_abbr(b"IM", b"ImageMask").and_then(|x| x.as_bool());
    let num_colors = match im {
        // If we have an image mask then we don't have a colorspace
        Ok(true) => 1,
        _ => match get_abbr(b"CS", b"ColorSpace")? {
            // An Indexed colour space is an array, and each sample is an index into its table.
            Object::Array(array) if matches!(array.first().map(Object::as_name), Some(Ok(b"Indexed" | b"I"))) => 1,
            colorspace => match colorspace.as_name()? {
                b"DeviceGray" | b"G" | b"Gray" => 1,
                b"DeviceRGB" | b"RGB" => 3,
                b"DeviceRGBA" | b"RGBA" => 4,
                b"DeviceCMYK" | b"CMYK" => 4,
                b"Pattern" => {
                    log::warn!("Pattern colorspace is not allowed in inline images");
                    return Err(Error::InvalidInlineImage(String::from(
                        "Pattern colorspace is not allowed in inline images",
                    )));
                }
                _ => {
                    log::warn!("Colorspace of inline image not recognized / not yet implemented");
                    return Err(Error::Unimplemented("inline image colorspaces"));
                }
            },
        },
    };

    let length = bpc
        .checked_mul(num_colors)
        .and_then(|bits_per_pixel| width.checked_mul(bits_per_pixel))
        .and_then(|row_bits| row_bits.div_ceil(8).checked_mul(height))
        .ok_or_else(invalid_size)?;

    let (input, content) = match get_abbr(b"F", b"Filter") {
        Err(_) => {
            // no decompression needed as no filter was applied
            take(length)
                .parse(input)
                .map_err(|_: nom::Err<()>| crate::error::ParseError::EndOfInput)?
        }
        Ok(Object::Name(_filter)) => {
            log::warn!("Filters for inline images are not yet implemented");
            return Err(Error::Unimplemented("filters for inline images"));
        }
        Ok(Object::Array(_filters)) => {
            log::warn!("Filters for inline images are not yet implemented");
            return Err(Error::Unimplemented("filters for inline images"));
        }
        Ok(obj) => {
            log::warn!("Filter must be either a Name or and Array.");
            return Err(Error::ObjectType {
                expected: "Name or Array",
                found: obj.enum_variant(),
            });
        }
    };
    Ok((input, Stream::new(stream_dict, content.to_vec())))
}

fn _content(input: ParserInput) -> NomResult<Content<Vec<Operation>>> {
    delimited(
        content_space,
        map(many0(operation), |operations| Content { operations }),
        many0(terminated(comment, content_space)),
    )
    .parse(input)
}

pub fn content(input: ParserInput) -> Option<Content<Vec<Operation>>> {
    strip_nom(_content.parse(input))
}

/// The operations of a content stream, decoded one at a time with the grammar of
/// [`content`].
///
/// Yields exactly the operations `content` collects, in order, then an error where
/// `content` fails, so the operations of a large stream need not all be held at once.
pub struct ContentOperations<'a> {
    input: ParserInput<'a>,
    started: bool,
    finished: bool,
}

impl<'a> ContentOperations<'a> {
    pub(crate) fn new(input: ParserInput<'a>) -> Self {
        ContentOperations {
            input,
            started: false,
            finished: false,
        }
    }

    fn fail(&mut self) -> Option<crate::Result<Operation>> {
        self.finished = true;
        Some(Err(error::ParseError::InvalidContentStream.into()))
    }
}

impl Iterator for ContentOperations<'_> {
    type Item = crate::Result<Operation>;

    fn next(&mut self) -> Option<Self::Item> {
        if self.finished {
            return None;
        }
        if !self.started {
            self.started = true;
            match content_space(self.input) {
                Ok((input, ())) => self.input = input,
                Err(_) => return self.fail(),
            }
        }
        let len = self.input.len();
        match operation(self.input) {
            // `many0` fails when its parser succeeds without consuming input.
            Ok((input, _)) if input.len() == len => self.fail(),
            Ok((input, operation)) => {
                self.input = input;
                Some(Ok(operation))
            }
            // `many0` stops at a recoverable error, and `content` ignores the rest.
            Err(nom::Err::Error(_)) => {
                self.finished = true;
                None
            }
            Err(_) => self.fail(),
        }
    }
}

pub fn content_strict(input: ParserInput) -> Result<Content<Vec<Operation>>, error::ParseError> {
    let (rest, content) = _content
        .parse(input)
        .map_err(|_| error::ParseError::InvalidContentStream)?;
    if !rest.is_empty() {
        return Err(error::ParseError::InvalidContentStream);
    }
    Ok(content)
}
