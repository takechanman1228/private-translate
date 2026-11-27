use crate::cmap_section::{ArrayOfTargetStrings, CMapParseError, CMapSection, CodeLen, SourceCode, SourceRangeMapping};
use crate::parser::{NomResult, ParserInput, comment, dict_dup, dictionary, eol, hex_char, name};
use nom::Parser;
use nom::branch::alt;
pub use nom::bytes::complete::tag;
use nom::combinator::{map, opt};
use nom::error::ParseError;
use nom::multi::{fold_many_m_n, fold_many0, fold_many1, many_m_n, many0, many1, separated_list1};
use nom::sequence::{pair, preceded, separated_pair, terminated};
use nom::{character::complete::digit1, sequence::delimited};

impl<E> From<nom::Err<E>> for CMapParseError {
    fn from(err: nom::Err<E>) -> Self {
        match err {
            nom::Err::Incomplete(_) => CMapParseError::Incomplete,
            // normally nom::Err::Error is a recoverable error, but CMapParseError is the return type
            // so we assume that there are no more parsing branches to check
            nom::Err::Failure(_) | nom::Err::Error(_) => CMapParseError::Error,
        }
    }
}

pub(crate) fn parse(stream_content: ParserInput) -> Result<Vec<CMapSection>, CMapParseError> {
    let result = cmap_stream(stream_content);
    let result = result.map_err(CMapParseError::from)?;
    Ok(result.1)
}

fn cmap_stream(input: ParserInput) -> NomResult<Vec<CMapSection>> {
    delimited(
        cidinit_procset,
        cmap_resource_dictionary,
        (tag(&b"end"[..]), multispace0),
    )
    .parse(input)
}

fn space0(input: ParserInput) -> NomResult<()> {
    fold_many0(alt((tag(&b" "[..]), tag("\t"))), || {}, |_, _| ()).parse(input)
}

fn space1(input: ParserInput) -> NomResult<()> {
    fold_many1(alt((tag(&b" "[..]), tag("\t"))), || {}, |_, _| ()).parse(input)
}

fn multispace0(input: ParserInput) -> NomResult<()> {
    let space = tag(&b" "[..]).map(|_| ());
    let tab = tag("\t").map(|_| ());
    let eol = eol.map(|_| ());
    fold_many0(alt((space, tab, eol, comment)), || {}, |_, _| ()).parse(input)
}

fn multispace1(input: ParserInput) -> NomResult<()> {
    let space = tag(&b" "[..]).map(|_| ());
    let tab = tag("\t").map(|_| ());
    let eol = eol.map(|_| ());
    fold_many1(alt((space, tab, eol, comment)), || {}, |_, _| ()).parse(input)
}

fn cidinit_procset(input: ParserInput) -> NomResult<()> {
    (
        opt(tag("\u{FEFF}".as_bytes())),
        multispace0,
        tag(&b"/CIDInit"[..]),
        space0,
        alt((tag(&b"/ProcSet"[..]), tag(&b"/Procset"[..]))),
        space1,
        tag(&b"findresource"[..]),
        space1,
        tag(&b"begin"[..]),
        multispace1,
    )
        .parse(input)
        .map(|(i, _)| (i, ()))
}

fn cmap_resource_dictionary(input: ParserInput) -> NomResult<Vec<CMapSection>> {
    let begin_parser = (
        digit1,
        space1,
        tag(&b"dict"[..]),
        space1,
        tag(&b"begin"[..]),
        multispace1,
    );
    let end_parser = (tag(&b"end"[..]), multispace1);
    delimited(begin_parser, cmap_data, end_parser).parse(input)
}

fn cmap_data(input: ParserInput) -> NomResult<Vec<CMapSection>> {
    let cmap_end = (
        tag(&b"endcmap"[..]),
        multispace1,
        tag(&b"CMapName"[..]),
        space1,
        tag(&b"currentdict"[..]),
        space0,
        tag(&b"/CMap"[..]),
        space1,
        tag(&b"defineresource"[..]),
        space1,
        tag(&b"pop"[..]),
        multispace1,
    );
    delimited(
        (tag(&b"begincmap"[..]), multispace1),
        preceded(cmap_metadata, cmap_codespace_and_mappings),
        cmap_end,
    )
    .parse(input)
}

fn cmap_metadata(input: ParserInput) -> NomResult<()> {
    let metadata_parser = alt((
        cid_system_info,
        cmap_name,
        cmap_type,
        cmap_version,
        uidoffset,
        wmode,
        xuid,
    ));
    fold_many_m_n(1, 7, metadata_parser, || (), |_, _| ()).parse(input)
}

fn cid_system_info(input: ParserInput) -> NomResult<()> {
    // Note: Can array of CIDSystemInfo occur here?
    // Normally in cmap this can be an array, but can it be also if this is a ToUnicode cmap?
    (
        tag(&b"/CIDSystemInfo"[..]),
        multispace0,
        alt((terminated(dictionary, multispace0), terminated(dict_dup, multispace1))),
        tag(&b"def"[..]),
        multispace1,
    )
        .parse(input)
        .map(|(i, _)| (i, ()))
}

fn cmap_name(input: ParserInput) -> NomResult<()> {
    (
        tag(&b"/CMapName"[..]),
        space0,
        name,
        space1,
        tag(&b"def"[..]),
        multispace1,
    )
        .parse(input)
        .map(|(i, _)| (i, ()))
}

fn cmap_type(input: ParserInput) -> NomResult<()> {
    (
        tag(&b"/CMapType"[..]),
        space1,
        digit1,
        space1,
        tag(&b"def"[..]),
        multispace1,
    )
        .parse(input)
        .map(|(i, _)| (i, ()))
}

fn cmap_version(input: ParserInput) -> NomResult<()> {
    let version = (digit1, opt((tag(&b"."[..]), digit1)));
    (
        tag(&b"/CMapVersion"[..]),
        space1,
        version,
        space1,
        tag(&b"def"[..]),
        multispace1,
    )
        .parse(input)
        .map(|(i, _)| (i, ()))
}

fn wmode(input: ParserInput) -> NomResult<()> {
    (
        tag(&b"/WMode"[..]),
        space1,
        digit1,
        space1,
        tag(&b"def"[..]),
        multispace1,
    )
        .parse(input)
        .map(|(i, _)| (i, ()))
}

fn uidoffset(input: ParserInput) -> NomResult<()> {
    (
        tag(&b"/UIDOffset"[..]),
        space1,
        digit1,
        space1,
        tag(&b"def"[..]),
        multispace1,
    )
        .parse(input)
        .map(|(i, _)| (i, ()))
}

fn xuid(input: ParserInput) -> NomResult<()> {
    let array = (tag(&b"["[..]), separated_list1(space1, digit1), tag(&b"]"[..]));
    (tag(&b"/XUID"[..]), space1, array, space1, tag(&b"def"[..]), multispace1)
        .parse(input)
        .map(|(i, _)| (i, ()))
}

fn cmap_codespace_and_mappings(input: ParserInput) -> NomResult<Vec<CMapSection>> {
    many1(alt((codespace_range_section, bf_char_section, bf_range_section))).parse(input)
}

fn codespace_range_section(input: ParserInput) -> NomResult<CMapSection> {
    let begin_section = (digit1, space1, tag(&b"begincodespacerange"[..]), multispace1);
    let end_section = (tag(&b"endcodespacerange"[..]), multispace1);
    let parse_range = delimited(space0, code_range_pair, multispace1);
    let (rest_of_input, ranges_result) = delimited(begin_section, many1(parse_range), end_section).parse(input)?;
    Ok((rest_of_input, CMapSection::CsRange(ranges_result)))
}

fn code_range_pair(input: ParserInput) -> NomResult<(SourceCode, SourceCode, CodeLen)> {
    let (rest_of_input, ((code_begin, code_len_beg), (code_end, code_len_end))) =
        separated_pair(source_code, space0, source_code).parse(input)?;
    if code_len_beg != code_len_end {
        create_code_len_err(rest_of_input)
    } else {
        Ok((rest_of_input, (code_begin, code_end, code_len_beg)))
    }
}

fn create_code_len_err<'a, T, E: ParseError<ParserInput<'a>>>(input: ParserInput<'a>) -> Result<T, nom::Err<E>> {
    Err(nom::Err::Failure(nom::error::make_error(
        input,
        nom::error::ErrorKind::LengthValue,
    )))
}

fn source_code(input: ParserInput) -> NomResult<(SourceCode, CodeLen)> {
    let (rest_of_input, bytes) = delimited(tag(&b"<"[..]), many_m_n(1, 4, hex_char), tag(&b">"[..])).parse(input)?;
    let code_len = bytes.len();
    let source_code = bytes
        .into_iter()
        .rev()
        .zip(0..4)
        .map(|(byte, i)| 256u32.pow(i) * byte as u32)
        .sum();
    Ok((rest_of_input, (source_code, code_len as u8)))
}

fn hex_u16(input: ParserInput) -> NomResult<u16> {
    map(pair(hex_char, hex_char), |(h1, h2)| h1 as u16 * 256 + h2 as u16).parse(input)
}

fn bf_char_section(input: ParserInput) -> NomResult<CMapSection> {
    let begin_section = (digit1, space1, tag(&b"beginbfchar"[..]), multispace1);
    let end_section = (tag(&b"endbfchar"[..]), multispace1);
    let bf_char_line = delimited(space0, separated_pair(source_code, space0, target_string), multispace1);
    // Some real-world ToUnicode CMaps contain sections like `0 beginbfchar ... endbfchar`.
    // Accept empty sections to avoid failing extraction (specifically calling extract_text)
    let (rest_of_input, bf_char_mappings) = delimited(begin_section, many0(bf_char_line), end_section).parse(input)?;
    Ok((rest_of_input, CMapSection::BfChar(bf_char_mappings)))
}

fn target_string(input: ParserInput) -> NomResult<Vec<u16>> {
    // according to specification dstString can be up to 512 bytes
    // in ToUnicode cmap these should be 2-byte big endian Unicode values
    delimited(
        tag(&b"<"[..]),
        many_m_n(1, 256, terminated(hex_u16, multispace0)),
        tag(&b">"[..]),
    )
    .parse(input)
}

fn bf_range_section(input: ParserInput) -> NomResult<CMapSection> {
    let begin_section = (digit1, space1, tag(&b"beginbfrange"[..]), multispace1);
    let end_section = (tag(&b"endbfrange"[..]), multispace1);
    // Some real-world ToUnicode CMaps contain sections like `0 beginbfrange ... endbfrange`.
    // Accept empty sections to avoid failing extraction (specifically calling extract_text)
    let (rest_of_input, bf_range_mappings) =
        delimited(begin_section, many0(bf_range_line), end_section).parse(input)?;
    Ok((rest_of_input, CMapSection::BfRange(bf_range_mappings)))
}

fn bf_range_line(input: ParserInput) -> NomResult<SourceRangeMapping> {
    let bf_range_parser = separated_pair(
        code_range_pair,
        space0,
        alt((target_string.map(|res| vec![res]), range_target_array)),
    );
    delimited(space0, bf_range_parser, multispace1).parse(input)
}

fn range_target_array(input: ParserInput) -> NomResult<ArrayOfTargetStrings> {
    delimited(
        (tag(&b"["[..]), space0),
        separated_list1(space1, target_string),
        (space0, tag(&b"]"[..])),
    )
    .parse(input)
}
