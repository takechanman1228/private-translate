//! Splitting long texts into chunks, and translating them in order.
//!
//! Plain text joins clear hard wraps before translation. Short neighbouring
//! paragraphs are packed into one chunk, and long paragraphs are split at natural boundaries
//! (see [`Boundary`]). A candidate that loses the source's layout is withdrawn and translated
//! paragraph by paragraph. Separators between chunks are copied from the source.
//!
//! [`translate_document`] translates the chunks in order and passes the most recent
//! source/translation pairs as history, so terminology and style stay consistent.
//!
//! Chunks are kept far below what the context window allows. PLaMo 3 attends to only the last
//! 2048 tokens in most layers, and small models decoding greedily tend to lose their place and
//! loop when one output runs much longer than that. When an output still runs away (see
//! [`crate::repetition`]), it is withdrawn ([`Output::Truncate`]) and the chunk is translated
//! again in pieces of a few sentences; a piece that runs away again is retried sentence by
//! sentence, and a sentence with repetition penalized, cut if even that loops. Output that ran
//! away never enters the history, so one loop cannot seed the next.

use std::borrow::Cow;
use std::cell::{Cell, RefCell};
use std::ops::Range;
use std::time::Duration;

use pfml::{HistoryPair, Prompt, TranslationOptions};

use crate::profiles::ModelProfile;
use crate::units::{UnitRequest, UnitResult, UnitTuning};
use crate::{EngineError, Generation, Result, StopReason, TranslateStats, TranslationBackend};
use crate::{hy_mt2, index_translate};

/// How a long text is split, and how much history accompanies each chunk.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DocumentOptions {
    /// Upper bound for a chunk, in tokens. Chunks are also limited by the context window.
    pub max_chunk_tokens: usize,
    /// Upper bound for the history passed with each chunk, in tokens.
    pub max_history_tokens: usize,
    /// Text appended where output was cut ([`Progress::Cut`]), so the gap shows in the
    /// translation. Empty for none.
    pub cut_marker: String,
}

impl Default for DocumentOptions {
    fn default() -> Self {
        Self {
            max_chunk_tokens: 1024,
            max_history_tokens: 4096,
            cut_marker: String::new(),
        }
    }
}

/// Output of [`translate_document`], to be applied in order.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Output<'a> {
    /// Text to append.
    Text(&'a str),
    /// Keep only the first `len` bytes of the output so far while a candidate is withdrawn.
    Truncate(usize),
}

/// Progress reports of [`translate_document`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Progress {
    /// Chunk `index` of `count` is starting. `source` is its byte range in the text.
    Chunk {
        index: usize,
        count: usize,
        source: (usize, usize),
    },
    /// The output for chunk `index`, or for a piece of it being retried, ran away (`reason`) and
    /// is translated again in `pieces` smaller pieces.
    Retry {
        index: usize,
        reason: StopReason,
        pieces: usize,
    },
    /// A packed translation changed layout and is being retried in individual paragraphs.
    ParagraphFallback { index: usize, paragraphs: usize },
    /// The output for a piece of chunk `index` that could not be split further (usually one
    /// sentence) ran away even with repetition penalized, and was cut. `source` is the piece's
    /// byte range in the text, and `output` the range of what was kept of its translation in the
    /// output, including the cut marker.
    Cut {
        index: usize,
        reason: StopReason,
        source: (usize, usize),
        output: (usize, usize),
    },
}

/// Size of the pieces a chunk is translated in again after its output ran away. Small enough
/// that loops are rare, large enough to keep some context and need only a few prompts: on the
/// CPU, retrying a chunk sentence by sentence took minutes, mostly processing the history again
/// for each sentence.
const RETRY_PIECE_TOKENS: usize = 256;

/// Output length limit for a source of `source_tokens` tokens. Translations are rarely more than
/// twice as long as the source in tokens, so this only stops output that ran away.
fn max_output_tokens(source_tokens: usize) -> usize {
    3 * source_tokens + 128
}

/// Whether a finished translation is so much shorter than its source that the model must have
/// ended early and skipped the rest. Large chunks did this silently; translations are rarely
/// under half the source's length in tokens.
fn looks_incomplete(source_tokens: usize, output_tokens: usize) -> bool {
    source_tokens >= 200 && output_tokens * 4 < source_tokens
}

/// A piece of the source: text to translate, followed by whitespace to copy unchanged.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Chunk<'a> {
    pub text: &'a str,
    pub separator: &'a str,
}

/// Leading whitespace of a document, then its chunks.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Split<'a> {
    pub leading: &'a str,
    pub chunks: Vec<Chunk<'a>>,
}

/// Splits `text` so each chunk measures at most `max_tokens` with `count_tokens`.
pub fn split<'a>(
    text: &'a str,
    max_tokens: usize,
    count_tokens: &dyn Fn(&str) -> usize,
) -> Split<'a> {
    split_with_layout(text, max_tokens, count_tokens, paragraphs(text, false))
}

fn split_with_layout<'a>(
    text: &'a str,
    max_tokens: usize,
    count_tokens: &dyn Fn(&str) -> usize,
    paragraphs: Vec<(usize, usize, usize)>,
) -> Split<'a> {
    let body_start = text.len() - text.trim_start().len();
    let leading = &text[..body_start];

    // Paragraphs as (start, end, separator_end) byte ranges, split further where too large.
    let mut pieces = Vec::new();
    for range in paragraphs {
        refine(
            text,
            range,
            Boundary::LineAfterSentence,
            max_tokens,
            count_tokens,
            &mut pieces,
        );
    }

    // Pack neighbouring pieces while they fit. Token counts are nearly additive, so the pieces
    // are counted once and the packed text is measured only near the limit.
    let counts: Vec<usize> = pieces
        .iter()
        .map(|&(start, end, _)| count_tokens(&text[start..end]))
        .collect();
    let mut chunks = Vec::new();
    let mut i = 0;
    while i < pieces.len() {
        let (start, mut end, mut sep_end) = pieces[i];
        let mut estimate = counts[i];
        let mut j = i + 1;
        while j < pieces.len() {
            let (next_start, next_end, next_sep_end) = pieces[j];
            let next_estimate = estimate + count_tokens(&text[end..next_start]) + counts[j];
            if next_estimate > max_tokens - max_tokens / 16 {
                let exact = count_tokens(&text[start..next_end]);
                if exact > max_tokens {
                    break;
                }
                estimate = exact;
            } else {
                estimate = next_estimate;
            }
            end = next_end;
            sep_end = next_sep_end;
            j += 1;
        }
        chunks.push(Chunk {
            text: &text[start..end],
            separator: &text[end..sep_end],
        });
        i = j;
    }
    Split { leading, chunks }
}

/// Where a piece too large for a chunk is split, from the most to the least natural.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Boundary {
    /// A line break after the end of a sentence: a paragraph break in text that separates
    /// paragraphs with a single newline, as Japanese prose usually does.
    LineAfterSentence,
    /// The end of a sentence. Other line breaks are ordinary whitespace, as in hard-wrapped prose.
    Sentence,
    /// Any line break.
    Line,
    /// After a comma or other clause punctuation, or at whitespace.
    Clause,
    /// Anywhere: the last resort, for text such as minified data without any of the above.
    Characters,
}

/// Appends `range` to `out` if it fits in `max_tokens`, or else its parts split at `boundary`,
/// each refined further at the next boundary.
fn refine(
    text: &str,
    range: (usize, usize, usize),
    boundary: Boundary,
    max_tokens: usize,
    count_tokens: &dyn Fn(&str) -> usize,
    out: &mut Vec<(usize, usize, usize)>,
) {
    let (start, end, sep_end) = range;
    if count_tokens(&text[start..end]) <= max_tokens {
        out.push(range);
        return;
    }
    let (parts, next) = match boundary {
        Boundary::LineAfterSentence => (
            lines_after_sentences(text, start, end, sep_end),
            Some(Boundary::Sentence),
        ),
        Boundary::Sentence => (
            sentences(text, start, end, sep_end, false),
            Some(Boundary::Line),
        ),
        Boundary::Line => (
            sentences(text, start, end, sep_end, true),
            Some(Boundary::Clause),
        ),
        Boundary::Clause => (
            clauses(text, start, end, sep_end),
            Some(Boundary::Characters),
        ),
        Boundary::Characters => (
            by_characters(text, start, end, sep_end, max_tokens, count_tokens),
            None,
        ),
    };
    for part in parts {
        match next {
            Some(next) => refine(text, part, next, max_tokens, count_tokens, out),
            None => out.push(part),
        }
    }
}

/// Sentence-ending punctuation that ends a sentence wherever it occurs.
fn is_terminal(c: char) -> bool {
    matches!(c, '。' | '！' | '？' | '．')
}

/// Sentence-ending punctuation that ends a sentence only before whitespace (`1.5` does not).
fn is_latin_terminal(c: char) -> bool {
    matches!(c, '.' | '!' | '?')
}

/// Closing quotes and brackets, which belong to the sentence they close.
fn is_closing(c: char) -> bool {
    matches!(c, '」' | '』' | '）' | ')' | '"' | '\'' | '”' | '’')
}

/// Whether a line ending with `line` (trailing whitespace removed) ends a sentence: terminal
/// punctuation, possibly inside closing quotes or brackets, or a Japanese closing quote.
fn ends_sentence(line: &str) -> bool {
    let core = line.trim_end_matches(is_closing);
    let japanese_quote = line[core.len()..].contains(['」', '』']);
    japanese_quote
        || core
            .chars()
            .last()
            .is_some_and(|c| is_terminal(c) || is_latin_terminal(c))
}

/// Ranges within `text[start..end]` split after clause punctuation and at whitespace, the last
/// one taking `sep_end`.
fn clauses(text: &str, start: usize, end: usize, sep_end: usize) -> Vec<(usize, usize, usize)> {
    let mut out = Vec::new();
    let mut piece_start = start;
    let mut chars = text[start..end].char_indices().peekable();
    while let Some((offset, c)) = chars.next() {
        let at = start + offset;
        let (piece_end, mut separator_end) = if c.is_whitespace() {
            (at, at + c.len_utf8())
        } else if matches!(c, '、' | '，' | ',' | ';' | '；' | ':' | '：') {
            (at + c.len_utf8(), at + c.len_utf8())
        } else {
            continue;
        };
        while let Some(&(next, c)) = chars.peek()
            && c.is_whitespace()
        {
            separator_end = start + next + c.len_utf8();
            chars.next();
        }
        if piece_end > piece_start && separator_end < end {
            out.push((piece_start, piece_end, separator_end));
            piece_start = separator_end;
        }
    }
    out.push((piece_start, end, sep_end));
    out
}

/// `text[start..end]` cut into pieces of equal character counts that fit in `max_tokens`, the
/// last one taking `sep_end`.
fn by_characters(
    text: &str,
    start: usize,
    end: usize,
    sep_end: usize,
    max_tokens: usize,
    count_tokens: &dyn Fn(&str) -> usize,
) -> Vec<(usize, usize, usize)> {
    let slice = &text[start..end];
    let chars = slice.chars().count();
    let tokens = count_tokens(slice).max(1);
    // A little under the average, as tokens are not spread evenly.
    let per_piece = (chars * max_tokens * 7 / 8 / tokens).max(1);
    let mut out = Vec::new();
    let mut piece_start = start;
    for (n, (offset, _)) in slice.char_indices().enumerate() {
        if n > 0 && n % per_piece == 0 {
            out.push((piece_start, start + offset, start + offset));
            piece_start = start + offset;
        }
    }
    out.push((piece_start, end, sep_end));
    out
}

/// Ranges within `text[start..end]` split at line breaks that follow the end of a sentence, the
/// last one taking `sep_end`.
fn lines_after_sentences(
    text: &str,
    start: usize,
    end: usize,
    sep_end: usize,
) -> Vec<(usize, usize, usize)> {
    let mut out = Vec::new();
    let mut piece_start = start;
    let mut i = start;
    while let Some(offset) = text[i..end].find('\n') {
        let newline = i + offset;
        let line = text[piece_start..newline].trim_end();
        let rest = &text[newline..end];
        let next_start = newline + (rest.len() - rest.trim_start().len());
        if ends_sentence(line) && next_start < end && !line.is_empty() {
            out.push((piece_start, piece_start + line.len(), next_start));
            piece_start = next_start;
        }
        i = next_start.max(newline + 1);
    }
    out.push((piece_start, end, sep_end));
    out
}

/// Paragraph ranges: (start, end of text, end of following whitespace).
fn paragraphs(text: &str, unicode_whitespace: bool) -> Vec<(usize, usize, usize)> {
    paragraphs_at(text, unicode_whitespace, |_| false)
}

fn paragraphs_at(
    text: &str,
    unicode_whitespace: bool,
    single_boundary: impl Fn(usize) -> bool,
) -> Vec<(usize, usize, usize)> {
    let mut out = Vec::new();
    let mut start = text.len() - text.trim_start().len();
    let bytes = text.as_bytes();
    let mut i = start;
    while i < bytes.len() {
        if bytes[i] == b'\n' {
            // A blank line: a newline followed by optional spaces and another newline.
            let after_line = &text[i + 1..];
            let next = if unicode_whitespace {
                after_line.trim_start_matches(|c: char| c != '\n' && c.is_whitespace())
            } else {
                after_line.trim_start_matches([' ', '\t', '\r'])
            };
            let j = text.len() - next.len();
            if (j < bytes.len() && bytes[j] == b'\n') || single_boundary(i) {
                let end = trim_end(text, start, i);
                let separator = &text[i..];
                let next = if unicode_whitespace {
                    separator.trim_start()
                } else {
                    separator.trim_ascii_start()
                };
                let sep_end = text.len() - next.len();
                if end > start {
                    out.push((start, end, sep_end));
                }
                start = sep_end;
                i = sep_end;
                continue;
            }
        }
        i += 1;
    }
    if start < text.len() {
        let end = trim_end(text, start, text.len());
        if end > start {
            out.push((start, end, text.len()));
        }
    }
    out
}

/// A plain-text document with wrap separators removed and byte offsets back into the source.
struct Reflowed<'a> {
    text: Cow<'a, str>,
    /// (Offset just after a replacement, total number of source bytes removed so far).
    offsets: Vec<(usize, usize)>,
    /// Offsets of remaining single newlines identified as intentional.
    boundaries: Vec<usize>,
    joined_lines: Vec<Range<usize>>,
}

impl<'a> Reflowed<'a> {
    fn new(source: &'a str) -> Self {
        let mut output = String::new();
        let mut offsets = Vec::new();
        let mut boundaries = Vec::new();
        let mut joined_lines = Vec::new();
        let mut copied = 0;
        for (start, end, _) in paragraphs(source, true) {
            // Whitespace before the first line is included in the preceding separator.
            let indent = source[..start].rsplit('\n').next().unwrap_or("");
            let block = &source[start..end];
            let breaks = classify_breaks(block, indent);
            output.push_str(&source[copied..start]);
            copied = start;
            let mut line_start = output.len();
            let mut joined = false;
            for (range, kind) in breaks {
                output.push_str(&source[copied..start + range.start]);
                match kind {
                    LineBreak::Wrap(glue) => {
                        output.push_str(glue);
                        offsets.push((output.len(), start + range.end - output.len()));
                        joined = true;
                    }
                    LineBreak::Boundary | LineBreak::Unknown => {
                        if joined {
                            joined_lines.push(line_start..output.len());
                            joined = false;
                        }
                        if kind == LineBreak::Boundary {
                            // CRLF and trailing spaces are part of the separator.
                            boundaries
                                .push(output.len() + block[range.clone()].find('\n').unwrap());
                        }
                        output.push_str(&block[range.clone()]);
                        line_start = output.len();
                    }
                }
                copied = start + range.end;
            }
            output.push_str(&source[copied..end]);
            if joined {
                joined_lines.push(line_start..output.len());
            }
            copied = end;
        }
        let text = if offsets.is_empty() {
            Cow::Borrowed(source)
        } else {
            output.push_str(&source[copied..]);
            Cow::Owned(output)
        };
        Self {
            text,
            offsets,
            boundaries,
            joined_lines,
        }
    }

    fn source_offset(&self, offset: usize) -> usize {
        let after = self.offsets.partition_point(|&(at, _)| at <= offset);
        offset + after.checked_sub(1).map_or(0, |i| self.offsets[i].1)
    }

    fn was_reflowed(&self) -> bool {
        !self.offsets.is_empty()
    }

    /// `text` is a slice of the reflowed document.
    fn offset_of(&self, text: &str) -> usize {
        text.as_ptr() as usize - self.text.as_ptr() as usize
    }

    fn is_joined_line(&self, text: &str) -> bool {
        let start = self.offset_of(text);
        let i = self
            .joined_lines
            .partition_point(|range| range.end <= start);
        self.joined_lines
            .get(i)
            .is_some_and(|range| range.start <= start && start + text.len() <= range.end)
    }

    fn paragraphs(&self, text: &str) -> Vec<(usize, usize, usize)> {
        let offset = self.offset_of(text);
        paragraphs_at(text, self.was_reflowed(), |at| {
            self.was_reflowed() && self.boundaries.binary_search(&(offset + at)).is_ok()
        })
    }

    fn paragraphs_for_retry<'t>(
        &self,
        source: &'t str,
        translation: &str,
    ) -> Option<Vec<Chunk<'t>>> {
        if !source.contains('\n') {
            return None;
        }
        let unicode_whitespace = self.was_reflowed();
        let mut source_paragraphs = self.paragraphs(source);
        let contains_internal_break =
            |&(start, end, _): &(usize, usize, usize)| source[start..end].contains('\n');
        if !unicode_whitespace {
            // Some plain text intentionally uses one line per paragraph. Sentence ends
            // establish that pattern only when every internal break follows one.
            source_paragraphs = source_paragraphs
                .into_iter()
                .flat_map(|(start, end, sep_end)| {
                    lines_after_sentences(source, start, end, sep_end)
                })
                .collect();
            if source_paragraphs.iter().any(contains_internal_break) {
                source_paragraphs = paragraphs(source, false);
            }
        }
        if source_paragraphs.len() <= 1 {
            return None;
        }

        let has_internal_breaks = source_paragraphs.iter().any(contains_internal_break);
        let single_boundaries = unicode_whitespace || !has_internal_breaks;
        let translated_paragraphs = paragraphs_at(translation.trim(), unicode_whitespace, |_| {
            single_boundaries
        });
        // A retained wrap can survive or disappear in translation. In that case,
        // only a shortfall in the number of possible boundaries is conclusive.
        if translated_paragraphs.len() == source_paragraphs.len()
            || (unicode_whitespace
                && has_internal_breaks
                && translated_paragraphs.len() > source_paragraphs.len())
        {
            return None;
        }
        Some(
            source_paragraphs
                .into_iter()
                .map(|(start, end, sep_end)| Chunk {
                    text: &source[start..end],
                    separator: &source[end..sep_end],
                })
                .collect(),
        )
    }
}

fn cjk(c: char) -> bool {
    matches!(
        c,
        '\u{2e80}'..='\u{2fff}' | '\u{3000}'..='\u{30ff}' | '\u{31f0}'..='\u{31ff}'
            | '\u{3400}'..='\u{9fff}' | '\u{ac00}'..='\u{d7af}' | '\u{f900}'..='\u{faff}'
            | '\u{ff01}'..='\u{ff9f}' | '\u{20000}'..='\u{323af}'
    )
}

fn list_marker(line: &str) -> bool {
    let first = line.chars().next().unwrap_or_default();
    if matches!(first, '•' | '◦' | '●' | '○' | '▪' | '‣' | '・')
        || ["- ", "+ ", "* ", "— ", "－ "]
            .iter()
            .any(|prefix| line.starts_with(prefix))
    {
        return true;
    }

    // Numbered and lettered markers are distinct from a number or quotation in running text.
    let marker = line.split_whitespace().next().unwrap_or("");
    let label = |label: &str| {
        let label = label.trim_start_matches(['(', '（']);
        !label.is_empty()
            && (label.chars().all(char::is_numeric)
                || (label.len() == 1 && label.as_bytes()[0].is_ascii_alphabetic())
                || label.chars().all(|c| "IVXLCDMivxlcdm".contains(c)))
    };
    let numbered_prefix = line
        .trim_start_matches(['(', '（'])
        .trim_start_matches(char::is_numeric);
    marker
        .strip_suffix(['.', ')', '．', '）', '、'])
        .is_some_and(label)
        || (line.chars().next().is_some_and(char::is_numeric)
            && numbered_prefix.starts_with(['．', '）', '、', ':']))
}

fn structural_line(line: &str) -> bool {
    let marker = line.split_whitespace().next().unwrap_or("");
    line.starts_with(['>', '＞', '※', '†', '‡'])
        || line.contains(['|', '│', '{', '}', '`', '=', '\\'])
        || line.contains("  ")
        || line.contains("-->")
        || (line.ends_with(';') && line.contains(['(', ')', '[', ']']))
        || marker
            .strip_prefix('[')
            .and_then(|word| word.strip_suffix(']'))
            .is_some_and(|word| !word.is_empty() && word.chars().all(char::is_numeric))
        || line.chars().all(char::is_numeric)
        || timestamp_prefix(line, marker)
        || line.split_once([':', '：']).is_some_and(|(speaker, rest)| {
            !rest.is_empty()
                && !speaker.is_empty()
                && speaker.chars().count() <= 16
                && speaker.chars().all(char::is_alphabetic)
        })
}

fn timestamp_prefix(line: &str, first_word: &str) -> bool {
    let looks_like_time = |word: &str| {
        matches!(
            word.trim_start_matches('[').as_bytes(),
            [b'0'..=b'9', b':', b'0'..=b'9', b'0'..=b'9', ..]
                | [b'0'..=b'9', b'0'..=b'9', b':', b'0'..=b'9', b'0'..=b'9', ..]
        )
    };
    if looks_like_time(first_word) {
        return true;
    }
    let date = first_word.trim_start_matches('[');
    if !matches!(
        date.as_bytes(),
        [
            b'0'..=b'9',
            b'0'..=b'9',
            b'0'..=b'9',
            b'0'..=b'9',
            b'-',
            b'0'..=b'9',
            b'0'..=b'9',
            b'-',
            b'0'..=b'9',
            b'0'..=b'9',
            ..
        ]
    ) {
        return false;
    }
    date.as_bytes().get(10) == Some(&b'T')
        || line[first_word.len()..]
            .split_whitespace()
            .next()
            .is_some_and(|word| {
                looks_like_time(word)
                    || matches!(word, "TRACE" | "DEBUG" | "INFO" | "WARN" | "ERROR")
            })
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum LineBreak {
    Wrap(&'static str),
    Boundary,
    Unknown,
}

fn continues_sentence(line: &str, next: &str, is_cjk: bool) -> bool {
    if ends_sentence(line) {
        return false;
    }
    let tail = line.chars().next_back().unwrap_or_default();
    let first = next.chars().next().unwrap_or_default();
    if is_cjk {
        (matches!(first, 'ぁ'..='ゖ') || "がをにへとでのはもや、，".contains(tail))
            && cjk(first)
            && !first.is_numeric()
            && !next.starts_with(['「', '『', '（', '【', '〈', '《'])
    } else {
        first.is_lowercase() || (matches!(tail, '-' | '\u{ad}') && first.is_alphabetic())
    }
}

/// Estimates the column width using at least three comparable lines that could wrap forward.
fn column_width(
    widths: &[usize],
    structural: &[bool],
    items: &[bool],
    continuations: &[bool],
    minimum: usize,
    near_width: usize,
) -> Option<usize> {
    let mut candidates: Vec<_> = (0..continuations.len())
        .filter(|&i| !structural[i] && !structural[i + 1] && !items[i + 1] && widths[i] >= minimum)
        .collect();
    candidates.sort_unstable_by_key(|&i| std::cmp::Reverse(widths[i]));
    candidates.windows(3).find_map(|group| {
        let width = widths[group[0]];
        (widths[group[2]] * 100 >= width * near_width && group.iter().any(|&i| continuations[i]))
            .then_some(width)
    })
}

fn classify_breaks(block: &str, first_indent: &str) -> Vec<(Range<usize>, LineBreak)> {
    let raw: Vec<_> = block.lines().collect();
    if raw.len() < 2 {
        return Vec::new();
    }
    let lines: Vec<_> = raw.iter().map(|line| line.trim()).collect();
    let widths: Vec<_> = lines.iter().map(|line| line.chars().count()).collect();
    let is_cjk = block.chars().filter(|&c| cjk(c)).count() * 2 > widths.iter().sum::<usize>();
    let indents: Vec<_> = std::iter::once(first_indent)
        .chain(
            raw[1..]
                .iter()
                .map(|line| &line[..line.len() - line.trim_start().len()]),
        )
        .collect();
    let structural: Vec<_> = raw
        .iter()
        .enumerate()
        .map(|(i, line)| {
            line.contains('\t')
                || (i == 0 && first_indent.contains('\t'))
                || structural_line(lines[i])
        })
        .collect();
    let items: Vec<_> = lines.iter().map(|line| list_marker(line)).collect();
    let continuations: Vec<_> = lines
        .windows(2)
        .map(|pair| continues_sentence(pair[0], pair[1], is_cjk))
        .collect();
    let (minimum, near_width) = if is_cjk { (22, 90) } else { (44, 84) };
    let width = column_width(
        &widths,
        &structural,
        &items,
        &continuations,
        minimum,
        near_width,
    );
    let full = |i: usize| {
        width.is_some_and(|width| {
            widths[i] * 100 >= width * near_width && widths[i] * near_width <= width * 100
        })
    };
    let first_line_indent = |i: usize| {
        i + 1 < lines.len()
            && !structural[i]
            && !structural[i + 1]
            && !items[i]
            && !items[i + 1]
            && indents[i].len() > indents[i + 1].len()
            && indents[i].starts_with(indents[i + 1])
            && indents[i].chars().all(|c| matches!(c, ' ' | '\u{3000}'))
            && (full(i) || (widths[i] >= minimum && continuations[i]))
            && (is_cjk
                || lines[i]
                    .chars()
                    .find(|c| c.is_alphabetic())
                    .is_some_and(char::is_uppercase))
    };
    let mut start = 0;
    let mut item_indent = None;
    let mut breaks = Vec::new();
    for i in 0..lines.len() - 1 {
        let end = start + raw[i].trim_end().len();
        let next_start = start
            + raw[i].len()
            + if block.as_bytes()[start + raw[i].len()] == b'\r' {
                2
            } else {
                1
            };
        let indent = indents[i];
        let next_indent = indents[i + 1];
        if items[i] && !structural[i] {
            item_indent = Some(indent);
        }
        let in_item = item_indent.is_some();
        let wraps_item = item_indent.is_some_and(|base| {
            !items[i + 1]
                && !structural[i + 1]
                && next_indent.starts_with(base)
                && (items[i] || next_indent == indent)
                && (full(i)
                    || continuations[i]
                    || (items[i] && next_indent.len() > base.len() && !ends_sentence(lines[i])))
        });
        let starts_paragraph = !wraps_item && ends_sentence(lines[i]) && first_line_indent(i + 1);
        let starts_heading_block = i == 0
            && width.is_some_and(|width| widths[i] * 2 < width)
            && full(i + 1)
            && !continuations[i];
        let first_line = first_line_indent(i);
        let wraps_prose = !in_item
            && (indent == next_indent || first_line)
            && ((full(i)
                && (full(i + 1) || !ends_sentence(lines[i]) || ends_sentence(lines[i + 1])))
                || (first_line && continuations[i]));
        let mut join_start = end;
        let kind = if items[i + 1] || starts_paragraph {
            LineBreak::Boundary
        } else if structural[i] || structural[i + 1] {
            if ends_sentence(lines[i]) || (structural[i] && structural[i + 1]) {
                LineBreak::Boundary
            } else {
                LineBreak::Unknown
            }
        } else if wraps_item || wraps_prose {
            let tail = lines[i].chars().next_back().unwrap();
            let first = lines[i + 1].chars().next().unwrap();
            let glue = match tail {
                '\u{ad}' if first.is_alphabetic() => {
                    join_start -= tail.len_utf8();
                    ""
                }
                '-' if first.is_alphanumeric() => "",
                _ => word_separator(tail, first),
            };
            LineBreak::Wrap(glue)
        } else if starts_heading_block || (width.is_some() && !full(i) && ends_sentence(lines[i])) {
            LineBreak::Boundary
        } else {
            LineBreak::Unknown
        };
        let join_end = if matches!(kind, LineBreak::Wrap(_)) {
            next_start + next_indent.len()
        } else {
            next_start
        };
        breaks.push((join_start..join_end, kind));
        start = next_start;
        if !wraps_item {
            item_indent = None;
        }
    }
    breaks
}

fn word_separator(left: char, right: char) -> &'static str {
    if cjk(left) && cjk(right) { "" } else { " " }
}

/// Coalesces model-generated newline runs across streamed token boundaries.
struct LineOutput {
    pending: String,
    previous: Option<char>,
    min_newlines: usize,
}

impl LineOutput {
    fn new(min_newlines: usize) -> Self {
        Self {
            pending: String::new(),
            previous: None,
            min_newlines,
        }
    }

    fn push(&mut self, mut text: &str, write: &mut dyn FnMut(&str)) {
        while !text.is_empty() {
            let rest = text.trim_start();
            self.pending.push_str(&text[..text.len() - rest.len()]);
            if rest.is_empty() {
                break;
            }
            self.flush(rest.chars().next(), write);
            let newline = rest.find(['\r', '\n']);
            let content = rest[..newline.unwrap_or(rest.len())].trim_end();
            write(content);
            self.previous = content.chars().next_back();
            text = &rest[content.len()..];
        }
    }

    fn flush(&mut self, next: Option<char>, write: &mut dyn FnMut(&str)) {
        if self.pending.is_empty() {
            return;
        }
        let breaks = self.pending.bytes().filter(|&c| c == b'\n').count();
        let has_break =
            breaks >= self.min_newlines || (self.min_newlines == 1 && self.pending.contains('\r'));
        if !has_break {
            write(&self.pending);
        } else if let (Some(left), Some(right)) = (self.previous, next) {
            write(word_separator(left, right));
        }
        self.pending.clear();
    }
}

fn flatten_breaks(text: &str, min_newlines: usize) -> String {
    let mut flattened = String::with_capacity(text.len());
    let mut write = |text: &str| flattened.push_str(text);
    let mut line = LineOutput::new(min_newlines);
    line.push(text, &mut write);
    line.flush(None, &mut write);
    flattened
}

/// Splits `text` into its sentences without packing them, to translate a chunk piece by piece.
pub fn sentences_of(text: &str) -> Split<'_> {
    let body_start = text.len() - text.trim_start().len();
    let mut chunks = Vec::new();
    for (start, end, sep_end) in paragraphs(text, false) {
        for (s, e, se) in sentences(text, start, end, sep_end, false) {
            chunks.push(Chunk {
                text: &text[s..e],
                separator: &text[e..se],
            });
        }
    }
    Split {
        leading: &text[..body_start],
        chunks,
    }
}

/// Sentence ranges within `text[start..end]`, the last one taking `sep_end`.
///
/// Sentences end at terminal punctuation. With `at_newlines`, every line break ends one as well;
/// otherwise a line break is ordinary whitespace, as in hard-wrapped prose.
fn sentences(
    text: &str,
    start: usize,
    end: usize,
    sep_end: usize,
    at_newlines: bool,
) -> Vec<(usize, usize, usize)> {
    let mut out = Vec::new();
    let mut sentence_start = start;
    let mut chars = text[start..end].char_indices().peekable();
    while let Some((offset, c)) = chars.next() {
        let is_terminal = is_terminal(c);
        let is_latin_terminal =
            is_latin_terminal(c) && chars.peek().is_none_or(|&(_, next)| next.is_whitespace());
        let is_newline = at_newlines && c == '\n';
        if !(is_terminal || is_latin_terminal || is_newline) {
            continue;
        }
        let sentence_end = start + offset + c.len_utf8();
        // Absorb closing quotes and brackets right after the terminator.
        let mut after = sentence_end;
        while let Some(&(o, next)) = chars.peek() {
            if is_closing(next) {
                chars.next();
                after = start + o + next.len_utf8();
            } else {
                break;
            }
        }
        let text_end = trim_end(text, sentence_start, after);
        let mut ws_end = after;
        while let Some(&(o, next)) = chars.peek() {
            if next.is_whitespace() {
                chars.next();
                ws_end = start + o + next.len_utf8();
            } else {
                break;
            }
        }
        if ws_end >= end {
            break;
        }
        if text_end > sentence_start {
            out.push((sentence_start, text_end, ws_end));
        }
        sentence_start = ws_end;
    }
    if sentence_start < end {
        out.push((sentence_start, end, sep_end));
    } else if let Some(last) = out.last_mut() {
        last.2 = sep_end;
    }
    out
}

fn trim_end(text: &str, start: usize, end: usize) -> usize {
    start + text[start..end].trim_end().len()
}

/// Translates `text` chunk by chunk, streaming the output (including separators) to `sink`.
///
/// The context window `n_ctx` is shared between fixed parts (context, glossary), history,
/// the chunk, and the output. Of what remains after the fixed parts, a chunk gets at most a sixth
/// and history a third, which leaves the output room for about three times the chunk;
/// `document` caps both further.
#[allow(clippy::too_many_arguments)]
pub fn translate_document(
    backend: &mut dyn TranslationBackend,
    count_tokens: &dyn Fn(&str) -> usize,
    n_ctx: usize,
    document: &DocumentOptions,
    options: &TranslationOptions,
    text: &str,
    sink: &mut dyn FnMut(Output<'_>),
    on_progress: &mut dyn FnMut(Progress),
    is_cancelled: &dyn Fn() -> bool,
) -> Result<TranslateStats> {
    translate_text(
        backend,
        count_tokens,
        n_ctx,
        document,
        options,
        text,
        sink,
        on_progress,
        is_cancelled,
        false,
    )
}

/// Translates a TXT file with file-only prompt leak checks and ordinary TXT cut repair.
#[allow(clippy::too_many_arguments)]
pub fn translate_file_text(
    backend: &mut dyn TranslationBackend,
    count_tokens: &dyn Fn(&str) -> usize,
    n_ctx: usize,
    document: &DocumentOptions,
    options: &TranslationOptions,
    text: &str,
    sink: &mut dyn FnMut(Output<'_>),
    on_progress: &mut dyn FnMut(Progress),
    is_cancelled: &dyn Fn() -> bool,
) -> Result<TranslateStats> {
    translate_text(
        backend,
        count_tokens,
        n_ctx,
        document,
        options,
        text,
        sink,
        on_progress,
        is_cancelled,
        true,
    )
}

#[allow(clippy::too_many_arguments)]
fn translate_text(
    backend: &mut dyn TranslationBackend,
    count_tokens: &dyn Fn(&str) -> usize,
    n_ctx: usize,
    document: &DocumentOptions,
    options: &TranslationOptions,
    text: &str,
    sink: &mut dyn FnMut(Output<'_>),
    on_progress: &mut dyn FnMut(Progress),
    is_cancelled: &dyn Fn() -> bool,
    file: bool,
) -> Result<TranslateStats> {
    let (chunk_budget, history_budget) = budgets(count_tokens, n_ctx, document, options);
    let reflowed = Reflowed::new(text);
    let text = reflowed.text.as_ref();
    let split = split_with_layout(text, chunk_budget, count_tokens, reflowed.paragraphs(text));
    let source_range = |(start, end)| (reflowed.source_offset(start), reflowed.source_offset(end));
    let mut report = |progress| {
        on_progress(match progress {
            Progress::Chunk {
                index,
                count,
                source,
            } => Progress::Chunk {
                index,
                count,
                source: source_range(source),
            },
            Progress::Cut {
                index,
                reason,
                source,
                output,
            } => Progress::Cut {
                index,
                reason,
                source: source_range(source),
                output,
            },
            other => other,
        });
    };
    let recover_prompt_leaks = backend.is_hy_mt2_1_8b();
    let index_file = file && backend.profile() == ModelProfile::IndexTranslate;
    // The 1.8B model can translate an earlier passage from history in place of the current one.
    // Index can echo its history-bearing prompt into TXT output.
    let history_budget = if recover_prompt_leaks || index_file {
        0
    } else {
        history_budget
    };
    let mut translator = Translator::new(
        text,
        backend,
        count_tokens,
        document,
        options,
        history_budget,
        sink,
        &mut report,
        is_cancelled,
    );
    translator.stream = true;
    translator.reflowed = Some(&reflowed);
    translator.recover_prompt_leaks = recover_prompt_leaks;
    translator.index_file = index_file;
    if split.chunks.is_empty() {
        return Ok(translator.total);
    }
    translator.emit(split.leading);

    let count = split.chunks.len();
    for (index, chunk) in split.chunks.iter().enumerate() {
        // Chunks are slices of `text`.
        let start = chunk.text.as_ptr() as usize - text.as_ptr() as usize;
        (translator.on_progress)(Progress::Chunk {
            index,
            count,
            source: (start, start + chunk.text.len()),
        });
        let output_start = translator.emitted;
        let history_start = translator.history.len();
        let cuts = translator.total.cut;
        let reason = match translator.chunk(index, chunk.text) {
            Err(EngineError::PromptLeak) => {
                translator.truncate(output_start);
                translator.history.truncate(history_start);
                translator.history_costs.truncate(history_start);
                translator.total.cut = cuts;
                // There is no usable translation. TXT's ordinary cut repair restores the source.
                translator.mark_cut(index, chunk.text, output_start, StopReason::Incomplete);
                StopReason::Finished
            }
            result => result?,
        };
        translator.total.stop_reason = reason;
        if reason != StopReason::Finished {
            break;
        }
        translator.emit(chunk.separator);
    }
    Ok(translator.total)
}

/// Translates `segments` (separate texts, such as the paragraphs of a web page) in order, each
/// one like [`translate_document`] and with the earlier ones as history. `on_segment` receives
/// each translation when it is complete. Stops at the first segment that is cancelled or does
/// not fit.
#[allow(clippy::too_many_arguments)]
pub fn translate_segments(
    backend: &mut dyn TranslationBackend,
    count_tokens: &dyn Fn(&str) -> usize,
    n_ctx: usize,
    document: &DocumentOptions,
    options: &TranslationOptions,
    segments: &[String],
    on_segment: &mut dyn FnMut(usize, String),
    is_cancelled: &dyn Fn() -> bool,
) -> Result<TranslateStats> {
    let (chunk_budget, history_budget) = budgets(count_tokens, n_ctx, document, options);
    // The output of all segments; each one's translation is the part it added.
    let output = RefCell::new(String::new());
    let mut sink = |o: Output<'_>| {
        let mut output = output.borrow_mut();
        match o {
            Output::Text(text) => output.push_str(text),
            Output::Truncate(len) => output.truncate(len),
        }
    };
    let mut on_progress = |_: Progress| {};
    let mut translator = Translator::new(
        "",
        backend,
        count_tokens,
        document,
        options,
        history_budget,
        &mut sink,
        &mut on_progress,
        is_cancelled,
    );
    let mut chunk_index = 0;
    for (index, segment) in segments.iter().enumerate() {
        translator.text = segment;
        let start = translator.emitted;
        let split = split(segment, chunk_budget, count_tokens);
        translator.emit(split.leading);
        for chunk in &split.chunks {
            let reason = translator.chunk(chunk_index, chunk.text)?;
            chunk_index += 1;
            translator.total.stop_reason = reason;
            if reason != StopReason::Finished {
                return Ok(translator.total);
            }
            translator.emit(chunk.separator);
        }
        let translation = output.borrow()[start..].to_owned();
        on_segment(index, translation);
    }
    Ok(translator.total)
}

/// Translates each file unit, publishing only a complete result that passes validation.
#[allow(clippy::too_many_arguments)]
pub fn translate_units(
    backend: &mut dyn TranslationBackend,
    count_tokens: &dyn Fn(&str) -> usize,
    n_ctx: usize,
    options: &TranslationOptions,
    request: &UnitRequest,
    on_unit: &mut dyn FnMut(usize, UnitResult),
    is_cancelled: &dyn Fn() -> bool,
) -> Result<TranslateStats> {
    let document = DocumentOptions {
        max_chunk_tokens: request.max_unit_tokens.max(1),
        max_history_tokens: request.max_history_tokens,
        cut_marker: String::new(),
    };
    let (chunk_budget, history_budget) = budgets(count_tokens, n_ctx, &document, options);
    let tuning = &request.tuning;
    let contextual_options = tuning
        .contexts
        .iter()
        .map(|context| {
            let mut options = options.clone();
            let limit = (n_ctx / 12).min(512);
            let mut end = 0;
            for line in context.split_inclusive('\n') {
                let next = end + line.len();
                if count_tokens(&context[..next]) > limit {
                    break;
                }
                end = next;
            }
            if end > 0 {
                if !options.context.trim().is_empty() {
                    options.context.push('\n');
                }
                options.context.push_str(context[..end].trim());
            }
            let limits = budgets(count_tokens, n_ctx, &document, &options);
            (options, limits)
        })
        .collect::<Vec<_>>();
    let mut seen = std::collections::HashMap::<&str, String>::new();
    let mut history_group = None;
    let buffer = RefCell::new(String::new());
    let mut sink = |output: Output<'_>| match output {
        Output::Text(piece) => buffer.borrow_mut().push_str(piece),
        Output::Truncate(len) => buffer.borrow_mut().truncate(len),
    };
    let mut on_progress = |_: Progress| {};
    let mut translator = Translator::new(
        "",
        backend,
        count_tokens,
        &document,
        options,
        history_budget,
        &mut sink,
        &mut on_progress,
        is_cancelled,
    );
    let recover_prompt_leaks = tuning.recover_prompt_leaks && translator.backend.is_hy_mt2_1_8b();
    translator.index_file = translator.backend.profile() == ModelProfile::IndexTranslate;
    for (index, unit) in request.units.iter().enumerate() {
        if is_cancelled() {
            translator.total.stop_reason = StopReason::Cancelled;
            break;
        }
        let (current, (chunk_budget, history_budget)) = tuning
            .context_by_unit
            .get(index)
            .and_then(|&id| contextual_options.get(id))
            .map_or(
                (options, (chunk_budget, history_budget)),
                |(options, limits)| (options, *limits),
            );
        translator.options = current;
        translator.history_budget = history_budget;
        let group = tuning.history_by_unit.get(index);
        if group != history_group {
            translator.history.clear();
            translator.history_costs.clear();
            history_group = group;
        }
        let validate = |text: &str| {
            if let Some(observe) = &tuning.observe_candidate {
                observe(index, text);
            }
            !text.trim().is_empty() && (request.validate)(index, text)
        };
        if tuning.reuse_translations
            && let Some(text) = seen.get(unit.text.as_str()).filter(|text| {
                (!translator.index_file || !index_translate::contains_prompt_leak(&unit.text, text))
                    && validate(text)
            })
        {
            on_unit(
                index,
                UnitResult::Complete {
                    text: text.clone(),
                    retried: false,
                },
            );
            continue;
        }
        translator.text = &unit.text;
        // The Hy-MT2 1.8B stream guard is only for splittable units. Tagged attempts
        // and their plain fallback use the same post-generation check as other file attempts.
        translator.recover_prompt_leaks = recover_prompt_leaks && !unit.indivisible;
        let history_start = translator.history.len();
        let cut_start = translator.total.cut;
        let result = if unit.text.trim().is_empty() {
            Ok(UnitResult::Invalid)
        } else if unit.indivisible {
            translator.indivisible_unit(index, chunk_budget, &validate, tuning)
        } else {
            translator.plain_unit(index, chunk_budget, &buffer, &validate)
        };
        translator.truncate(0);
        if translator.total.stop_reason == StopReason::Cancelled || is_cancelled() {
            translator.total.stop_reason = StopReason::Cancelled;
            break;
        }
        let result = match result {
            Ok(result) => result,
            Err(EngineError::PromptTooLong { .. }) => UnitResult::TooLong,
            Err(EngineError::PromptLeak) => UnitResult::PromptLeak,
            Err(error) => return Err(error),
        };
        translator.total.cut = cut_start + usize::from(result == UnitResult::Cut);
        if !matches!(result, UnitResult::Complete { .. }) {
            translator.history.truncate(history_start);
            translator.history_costs.truncate(history_start);
        }
        if tuning.reuse_translations
            && let UnitResult::Complete { text, .. } = &result
        {
            seen.entry(unit.text.as_str())
                .or_insert_with(|| text.clone());
        }
        on_unit(index, result);
    }
    Ok(translator.total)
}

/// Chunk and history budgets: of what remains of `n_ctx` after the fixed parts (context and
/// glossary, plus a margin), a chunk gets at most a sixth and history a third, and `document`
/// caps both further.
fn budgets(
    count_tokens: &dyn Fn(&str) -> usize,
    n_ctx: usize,
    document: &DocumentOptions,
    options: &TranslationOptions,
) -> (usize, usize) {
    let fixed = count_tokens(&options.context)
        + options
            .glossary
            .iter()
            .map(|e| count_tokens(&e.source) + count_tokens(&e.target) + 2)
            .sum::<usize>()
        + 64;
    let available = n_ctx.saturating_sub(fixed).max(384);
    (
        (available / 6).min(document.max_chunk_tokens),
        (available / 3).min(document.max_history_tokens),
    )
}

enum Attempt {
    Translated(String, StopReason, usize),
    LeadingLeak,
}

struct Translator<'a> {
    /// The text being translated; every piece translated is a slice of it.
    text: &'a str,
    cut_marker: &'a str,
    backend: &'a mut dyn TranslationBackend,
    hy_mt2: bool,
    office_anchor_recovery: bool,
    stream: bool,
    reflowed: Option<&'a Reflowed<'a>>,
    restoring_layout: bool,
    recover_prompt_leaks: bool,
    index_file: bool,
    count_tokens: &'a dyn Fn(&str) -> usize,
    options: &'a TranslationOptions,
    sink: &'a mut dyn FnMut(Output<'_>),
    on_progress: &'a mut dyn FnMut(Progress),
    is_cancelled: &'a dyn Fn() -> bool,
    history: Vec<HistoryPair>,
    /// Tokens each history pair takes in a prompt, counted once.
    history_costs: Vec<usize>,
    history_budget: usize,
    /// Bytes of output so far.
    emitted: usize,
    total: TranslateStats,
}

impl<'a> Translator<'a> {
    #[allow(clippy::too_many_arguments)]
    fn new(
        text: &'a str,
        backend: &'a mut dyn TranslationBackend,
        count_tokens: &'a dyn Fn(&str) -> usize,
        document: &'a DocumentOptions,
        options: &'a TranslationOptions,
        history_budget: usize,
        sink: &'a mut dyn FnMut(Output<'_>),
        on_progress: &'a mut dyn FnMut(Progress),
        is_cancelled: &'a dyn Fn() -> bool,
    ) -> Self {
        let profile = backend.profile();
        Self {
            text,
            cut_marker: &document.cut_marker,
            hy_mt2: profile == ModelProfile::HyMt2,
            office_anchor_recovery: matches!(
                profile,
                ModelProfile::HyMt2 | ModelProfile::IndexTranslate
            ),
            stream: false,
            reflowed: None,
            restoring_layout: false,
            recover_prompt_leaks: false,
            index_file: false,
            backend,
            count_tokens,
            options,
            sink,
            on_progress,
            is_cancelled,
            history: Vec::new(),
            history_costs: Vec::new(),
            history_budget,
            emitted: 0,
            total: TranslateStats {
                prompt_tokens: 0,
                generated_tokens: 0,
                prompt_duration: Duration::ZERO,
                generation_duration: Duration::ZERO,
                stop_reason: StopReason::Finished,
                retries: 0,
                cut: 0,
                prompt_leaks: 0,
            },
        }
    }

    fn emit(&mut self, text: &str) {
        if !text.is_empty() {
            self.emitted += text.len();
            (self.sink)(Output::Text(text));
        }
    }

    fn truncate(&mut self, len: usize) {
        if len < self.emitted {
            self.emitted = len;
            (self.sink)(Output::Truncate(len));
        }
    }

    /// A leak detected after streaming gets one retry with the existing stronger sampler.
    fn generate(&mut self, source: &str, mut generation: Generation) -> Result<Attempt> {
        match self.generate_attempt(source, generation) {
            Err(EngineError::PromptLeak) if !generation.penalize_repetition => {
                self.total.retries += 1;
                generation.penalize_repetition = true;
                match self.generate_attempt(source, generation) {
                    Ok(Attempt::LeadingLeak) => Err(EngineError::PromptLeak),
                    result => result,
                }
            }
            result => result,
        }
    }

    /// Stream interactive text and buffer file candidates that need a post-generation check.
    fn generate_attempt(&mut self, source: &str, generation: Generation) -> Result<Attempt> {
        let recent = recent_history(&self.history, &self.history_costs, self.history_budget);
        let prompt = Prompt::new(self.options, source).with_history(recent);
        let mut translation = String::new();
        let start = self.emitted;
        let emitted = &mut self.emitted;
        let sink = &mut self.sink;
        let mut write = |text: &str| {
            if !text.is_empty() {
                *emitted += text.len();
                sink(Output::Text(text));
            }
        };
        let stream = (self.stream || !self.hy_mt2) && !self.index_file;
        let single_line = (self.restoring_layout && !source.contains('\n'))
            || self
                .reflowed
                .is_some_and(|reflowed| reflowed.is_joined_line(source));
        let mut line_output = single_line.then(|| LineOutput::new(1));
        let mut guard = self
            .recover_prompt_leaks
            .then(|| hy_mt2::PromptLeakStream::new(&prompt, generation.preserve_office_anchors));
        let stop_early = Cell::new(false);
        let is_cancelled = self.is_cancelled;
        let stats = self.backend.translate(
            &prompt,
            &generation,
            &mut |piece| {
                if piece.is_empty() || stop_early.get() {
                    return;
                }
                translation.push_str(piece);
                let ready = match &mut guard {
                    Some(guard) => match guard.push(&translation) {
                        Some(ready) => ready,
                        None => {
                            stop_early.set(true);
                            return;
                        }
                    },
                    None if stream => piece,
                    None => return,
                };
                if let Some(line) = &mut line_output {
                    line.push(ready, &mut write);
                } else {
                    write(ready);
                }
            },
            &|| stop_early.get() || is_cancelled(),
        )?;
        self.total.prompt_tokens += stats.prompt_tokens;
        self.total.generated_tokens += stats.generated_tokens;
        self.total.prompt_duration += stats.prompt_duration;
        self.total.generation_duration += stats.generation_duration;
        if stop_early.get() {
            self.total.prompt_leaks += 1;
            return if is_cancelled() {
                Ok(Attempt::Translated(
                    String::new(),
                    StopReason::Cancelled,
                    stats.generated_tokens,
                ))
            } else {
                Ok(Attempt::LeadingLeak)
            };
        }
        if self.index_file && index_translate::contains_prompt_leak(source, &translation) {
            self.total.prompt_leaks += 1;
            if stats.stop_reason != StopReason::Cancelled {
                return Err(EngineError::PromptLeak);
            }
            translation.clear();
        }
        if self.hy_mt2 {
            if (stream || stats.stop_reason != StopReason::Cancelled)
                && hy_mt2::contains_prompt_leak(
                    &prompt,
                    generation.preserve_office_anchors,
                    self.backend.hy_mt2_prompt_frame(),
                    &translation,
                )
            {
                self.truncate(start);
                self.total.prompt_leaks += 1;
                if stats.stop_reason != StopReason::Cancelled {
                    return Err(EngineError::PromptLeak);
                }
                line_output = None;
            } else if let Some(guard) = guard {
                let remaining = guard.remaining(&translation);
                if let Some(line) = &mut line_output {
                    line.push(remaining, &mut |text| self.emit(text));
                } else {
                    self.emit(remaining);
                }
            } else if !stream && stats.stop_reason != StopReason::Cancelled {
                self.emit(&translation);
            }
        }
        if let Some(line) = &mut line_output {
            line.flush(None, &mut |text| self.emit(text));
        }
        let mut reason = stats.stop_reason;
        if (single_line || self.restoring_layout) && translation.contains(['\r', '\n']) {
            let min_newlines = if single_line { 1 } else { 2 };
            let flattened = flatten_breaks(&translation, min_newlines);
            if flattened != translation {
                if self.index_file && index_translate::contains_prompt_leak(source, &flattened) {
                    self.total.prompt_leaks += 1;
                    return Err(EngineError::PromptLeak);
                }
                if !single_line && self.emitted == start + translation.len() {
                    self.truncate(start);
                    self.emit(&flattened);
                }
                if let StopReason::Repetition { kept } = &mut reason {
                    *kept = flatten_breaks(&translation[..*kept], min_newlines).len();
                }
                translation = flattened;
            }
        }
        if self.index_file && stats.stop_reason != StopReason::Cancelled {
            self.emit(&translation);
        }
        Ok(Attempt::Translated(
            translation,
            reason,
            stats.generated_tokens,
        ))
    }

    fn remember(&mut self, source: &str, translation: &str) {
        let translation = translation.trim();
        if self.history_budget > 0 && !translation.is_empty() {
            let cost = (self.count_tokens)(source) + (self.count_tokens)(translation) + 16;
            self.history.push(HistoryPair {
                source: source.to_owned(),
                translation: translation.to_owned(),
            });
            self.history_costs.push(cost);
        }
    }

    /// Keep tagged inputs whole, and commit only validated translations to history.
    fn indivisible_unit(
        &mut self,
        index: usize,
        budget: usize,
        validate: &dyn Fn(&str) -> bool,
        tuning: &UnitTuning,
    ) -> Result<UnitResult> {
        let source = self.text;
        let tokens = (self.count_tokens)(source);
        if tokens > budget {
            return Ok(UnitResult::TooLong);
        }
        let preserve_office_anchors = self.office_anchor_recovery
            && tuning.preserve_office_anchors
            && hy_mt2::has_anchors(source);
        let generation = Generation {
            preserve_office_anchors,
            ..Generation::greedy(max_output_tokens(tokens))
        };
        let first = self.unit_attempt(source, tokens, generation, validate)?;
        let invalid_anchors = preserve_office_anchors && first == UnitResult::Invalid;
        if !invalid_anchors && first != UnitResult::PromptLeak {
            return Ok(first);
        }

        self.total.retries += 1;
        let second = self.unit_attempt(
            source,
            tokens,
            Generation {
                penalize_repetition: true,
                ..generation
            },
            validate,
        )?;
        if self.total.stop_reason == StopReason::Cancelled {
            return Ok(second);
        }
        if (invalid_anchors || (preserve_office_anchors && second == UnitResult::Invalid))
            && !matches!(second, UnitResult::Complete { .. })
        {
            if let Some(fallback) = &tuning.anchor_fallback
                && let Some(plain) = (fallback.source)(index)
            {
                let tokens = (self.count_tokens)(&plain);
                match self.generate(&plain, Generation::greedy(max_output_tokens(tokens))) {
                    Ok(Attempt::Translated(_, StopReason::Cancelled, _)) => {
                        self.total.stop_reason = StopReason::Cancelled;
                    }
                    Ok(Attempt::Translated(text, StopReason::Finished, generated))
                        if !looks_incomplete(tokens, generated)
                            && (fallback.validate)(index, &text) =>
                    {
                        return Ok(UnitResult::Simplified { text });
                    }
                    Err(EngineError::PromptLeak) if self.index_file => {
                        return Ok(UnitResult::PromptLeak);
                    }
                    Ok(_) | Err(EngineError::PromptLeak | EngineError::PromptTooLong { .. }) => {}
                    Err(error) => return Err(error),
                }
            }
            // An unrepairable tag error retains the original paragraph.
            return Ok(UnitResult::Invalid);
        }
        Ok(second)
    }

    fn unit_attempt(
        &mut self,
        source: &str,
        tokens: usize,
        generation: Generation,
        validate: &dyn Fn(&str) -> bool,
    ) -> Result<UnitResult> {
        if (self.is_cancelled)() {
            self.total.stop_reason = StopReason::Cancelled;
            return Ok(UnitResult::Cut);
        }
        let start = self.emitted;
        let (translation, reason, generated) = match self.generate_attempt(source, generation) {
            Ok(Attempt::Translated(text, reason, generated)) => (text, reason, generated),
            Ok(Attempt::LeadingLeak) | Err(EngineError::PromptLeak) => {
                return Ok(UnitResult::PromptLeak);
            }
            Err(error) => return Err(error),
        };
        let result = match reason {
            StopReason::Cancelled => {
                self.total.stop_reason = reason;
                UnitResult::Cut
            }
            StopReason::ContextFull => UnitResult::TooLong,
            StopReason::Finished if !looks_incomplete(tokens, generated) => {
                if validate(&translation) {
                    self.remember(source, &translation);
                    return Ok(UnitResult::Complete {
                        text: translation,
                        retried: generation.penalize_repetition,
                    });
                }
                UnitResult::Invalid
            }
            _ => UnitResult::Cut,
        };
        if self.hy_mt2 {
            self.truncate(start);
        }
        Ok(result)
    }

    /// Plain units use the original chunk and recovery rules. Their history is committed by
    /// the caller only if the final unit passes validation.
    fn plain_unit(
        &mut self,
        index: usize,
        budget: usize,
        buffer: &RefCell<String>,
        validate: &dyn Fn(&str) -> bool,
    ) -> Result<UnitResult> {
        let cut_start = self.total.cut;
        let retry_start = self.total.retries;
        let split = split(self.text, budget, self.count_tokens);
        self.emit(split.leading);
        for chunk in &split.chunks {
            match self.chunk(index, chunk.text)? {
                StopReason::Finished => self.emit(chunk.separator),
                StopReason::ContextFull => return Ok(UnitResult::TooLong),
                reason => {
                    if reason == StopReason::Cancelled {
                        self.total.stop_reason = reason;
                    }
                    return Ok(UnitResult::Cut);
                }
            }
        }
        if self.total.cut != cut_start {
            return Ok(UnitResult::Cut);
        }
        let translation = buffer.borrow();
        if self.index_file && index_translate::contains_prompt_leak(self.text, &translation) {
            self.total.prompt_leaks += 1;
            return Ok(UnitResult::PromptLeak);
        }
        if validate(&translation) {
            Ok(UnitResult::Complete {
                text: translation.clone(),
                retried: self.total.retries != retry_start,
            })
        } else {
            Ok(UnitResult::Invalid)
        }
    }

    /// Translates chunk `index`, retrying it in smaller pieces if its output runs away.
    /// Returns `Finished`, `Cancelled`, or `ContextFull`.
    fn chunk(&mut self, index: usize, text: &str) -> Result<StopReason> {
        self.piece(index, text, Level::Chunk)
    }

    /// Translates `text`. If the output runs away, it is withdrawn and `text` is translated again
    /// in smaller pieces: a chunk in pieces of up to [`RETRY_PIECE_TOKENS`], such a piece sentence
    /// by sentence, and a single sentence with repetition penalized, cut if even that loops.
    fn piece(&mut self, index: usize, text: &str, level: Level) -> Result<StopReason> {
        let start = self.emitted;
        let source_tokens = (self.count_tokens)(text);
        let max_tokens = max_output_tokens(source_tokens);
        let (translation, mut reason, generated) =
            match self.generate(text, Generation::greedy(max_tokens)) {
                Ok(Attempt::Translated(text, reason, generated)) => (text, reason, generated),
                Ok(Attempt::LeadingLeak) => {
                    return self.retry_in_smaller_pieces(
                        index,
                        text,
                        level,
                        StopReason::Incomplete,
                        |this| this.penalized(index, text, max_tokens),
                    );
                }
                Err(EngineError::PromptLeak) if self.recover_prompt_leaks => {
                    return self.retry_in_smaller_pieces(
                        index,
                        text,
                        level,
                        StopReason::Incomplete,
                        |this| Ok(this.cut_leaked_piece(index, text)),
                    );
                }
                Err(error) => return Err(error),
            };
        if reason == StopReason::Finished {
            if !self.restoring_layout
                && let Some(paragraphs) = self
                    .reflowed
                    .and_then(|reflowed| reflowed.paragraphs_for_retry(text, &translation))
            {
                self.truncate(start);
                self.total.retries += 1;
                (self.on_progress)(Progress::ParagraphFallback {
                    index,
                    paragraphs: paragraphs.len(),
                });
                self.restoring_layout = true;
                let result = self.translate_pieces(index, &paragraphs, Level::Chunk);
                self.restoring_layout = false;
                return result;
            }
            if looks_incomplete(source_tokens, generated) {
                reason = StopReason::Incomplete;
            } else {
                self.remember(text, &translation);
            }
        }
        if !reason.is_runaway() {
            return Ok(reason);
        }

        self.truncate(start);
        self.retry_in_smaller_pieces(index, text, level, reason, |this| {
            this.penalized(index, text, max_tokens)
        })
    }

    fn retry_in_smaller_pieces(
        &mut self,
        index: usize,
        text: &str,
        level: Level,
        reason: StopReason,
        on_unsplittable: impl FnOnce(&mut Self) -> Result<StopReason>,
    ) -> Result<StopReason> {
        // The next level that actually splits `text`: translating the same input again with the
        // same history would only repeat the same output.
        let mut next = level;
        let pieces = loop {
            next = match next.smaller() {
                Some(next) => next,
                None => return on_unsplittable(self),
            };
            let pieces = match next {
                // `smaller` never returns `Chunk`.
                Level::Chunk | Level::Piece => split(text, RETRY_PIECE_TOKENS, self.count_tokens),
                Level::Sentence => sentences_of(text),
            };
            if pieces.chunks.len() > 1 {
                break pieces;
            }
        };
        self.total.retries += 1;
        (self.on_progress)(Progress::Retry {
            index,
            reason,
            pieces: pieces.chunks.len(),
        });
        self.emit(pieces.leading);
        self.translate_pieces(index, &pieces.chunks, next)
    }

    fn translate_pieces(
        &mut self,
        index: usize,
        pieces: &[Chunk<'_>],
        level: Level,
    ) -> Result<StopReason> {
        for piece in pieces {
            let reason = self.piece(index, piece.text, level)?;
            if reason != StopReason::Finished {
                return Ok(reason);
            }
            self.emit(piece.separator);
        }
        Ok(StopReason::Finished)
    }

    /// Translates `text`, which cannot be split further, with repetition penalized. Output that
    /// still runs away is cut.
    fn penalized(&mut self, index: usize, text: &str, max_tokens: usize) -> Result<StopReason> {
        self.total.retries += 1;
        let start = self.emitted;
        let generation = Generation {
            max_tokens,
            penalize_repetition: true,
            preserve_office_anchors: false,
        };
        let (translation, reason, _) = match self.generate(text, generation) {
            Ok(Attempt::Translated(text, reason, generated)) => (text, reason, generated),
            Ok(Attempt::LeadingLeak) => return Ok(self.cut_leaked_piece(index, text)),
            Err(EngineError::PromptLeak) if self.recover_prompt_leaks => {
                return Ok(self.cut_leaked_piece(index, text));
            }
            Err(error) => return Err(error),
        };
        match reason {
            StopReason::Finished => {
                self.remember(text, &translation);
                return Ok(reason);
            }
            StopReason::Cancelled | StopReason::ContextFull => return Ok(reason),
            // Keep the text worth keeping (see `StopReason::Repetition`).
            StopReason::Repetition { kept } => self.truncate(start + kept),
            // Keep the output, which ends wherever the limit cut it. (Backends never report
            // Incomplete.)
            StopReason::LengthLimit | StopReason::Incomplete => {}
        }
        self.mark_cut(index, text, start, reason);
        Ok(StopReason::Finished)
    }

    fn cut_leaked_piece(&mut self, index: usize, text: &str) -> StopReason {
        // Keep the validated translations of sibling pieces when this piece still leaks.
        self.mark_cut(index, text, self.emitted, StopReason::Incomplete);
        StopReason::Finished
    }

    fn mark_cut(&mut self, index: usize, text: &str, start: usize, reason: StopReason) {
        self.emit(self.cut_marker);
        self.total.cut += 1;
        let source_start = text.as_ptr() as usize - self.text.as_ptr() as usize;
        (self.on_progress)(Progress::Cut {
            index,
            reason,
            source: (source_start, source_start + text.len()),
            output: (start, self.emitted),
        });
    }
}

/// How finely a retried text is split.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Level {
    Chunk,
    /// Pieces of up to [`RETRY_PIECE_TOKENS`] tokens, at the same boundaries as chunks.
    Piece,
    Sentence,
}

impl Level {
    fn smaller(self) -> Option<Self> {
        match self {
            Self::Chunk => Some(Self::Piece),
            Self::Piece => Some(Self::Sentence),
            Self::Sentence => None,
        }
    }
}

/// The longest suffix of `history` whose `costs` (tokens per pair) fit in `budget`.
fn recent_history<'h>(
    history: &'h [HistoryPair],
    costs: &[usize],
    budget: usize,
) -> &'h [HistoryPair] {
    let mut used = 0;
    let mut start = history.len();
    for (i, cost) in costs.iter().enumerate().rev() {
        used += cost;
        if used > budget {
            break;
        }
        start = i;
    }
    &history[start..]
}

#[cfg(test)]
mod tests {
    use super::*;

    use std::collections::VecDeque;
    use std::sync::Arc;

    use crate::units::{AnchorFallback, Unit};

    fn chars(s: &str) -> usize {
        s.chars().count()
    }

    #[derive(Default)]
    struct IndexReplies<'a> {
        replies: VecDeque<(&'a str, bool, bool, &'a str)>,
        history_lengths: Vec<usize>,
    }

    impl TranslationBackend for IndexReplies<'_> {
        fn profile(&self) -> ModelProfile {
            ModelProfile::IndexTranslate
        }

        fn translate(
            &mut self,
            prompt: &Prompt<'_>,
            generation: &Generation,
            sink: &mut dyn FnMut(&str),
            _: &dyn Fn() -> bool,
        ) -> Result<TranslateStats> {
            let (source, penalized, anchors, output) =
                self.replies.pop_front().expect("extra generation");
            assert_eq!(prompt.text, source);
            assert!(!prompt.options.glossary.is_empty());
            assert_eq!(generation.penalize_repetition, penalized);
            assert_eq!(generation.preserve_office_anchors, anchors);
            self.history_lengths.push(prompt.history.len());
            sink(output);
            Ok(stats(StopReason::Finished, output))
        }
    }

    fn index_options() -> TranslationOptions {
        let mut options = TranslationOptions::new(pfml::LangCode::new("ja").unwrap());
        options.source = Some(pfml::LangCode::new("en").unwrap());
        options.glossary.push(pfml::GlossaryEntry {
            source: "Book".into(),
            target: "本".into(),
        });
        options
    }

    fn run_index_units(
        sources: &[&str],
        tagged: bool,
        tuning: UnitTuning,
        replies: impl Into<VecDeque<(&'static str, bool, bool, &'static str)>>,
    ) -> (Vec<UnitResult>, TranslateStats) {
        let mut backend = IndexReplies {
            replies: replies.into(),
            ..Default::default()
        };
        let request = UnitRequest {
            units: sources
                .iter()
                .map(|text| Unit {
                    text: (*text).into(),
                    indivisible: tagged,
                })
                .collect(),
            tuning,
            max_unit_tokens: 1024,
            max_history_tokens: 0,
            validate: Arc::new(move |_, text| !tagged || text.starts_with(r#"<a i="1">"#)),
        };
        let mut units = Vec::new();
        let stats = translate_units(
            &mut backend,
            &chars,
            8192,
            &index_options(),
            &request,
            &mut |_, result| units.push(result),
            &|| false,
        )
        .unwrap();
        assert!(backend.replies.is_empty());
        (units, stats)
    }

    #[test]
    fn index_retries_a_leak_in_an_untagged_unit_with_a_glossary() {
        let (units, stats) = run_index_units(
            &["Book"],
            false,
            UnitTuning::default(),
            [
                ("Book", false, false, "〖源文〗 本"),
                ("Book", true, false, "本"),
            ],
        );
        assert_eq!(
            units,
            [UnitResult::Complete {
                text: "本".into(),
                retried: true
            }]
        );
        assert_eq!((stats.prompt_leaks, stats.retries), (1, 1));
    }

    #[test]
    fn index_rejects_leaks_in_tagged_candidates_and_single_line_fallbacks() {
        let tagged = r#"<a i="1">Book</a>"#;
        let tuning = UnitTuning {
            preserve_office_anchors: true,
            anchor_fallback: Some(AnchorFallback {
                source: Arc::new(|_| Some("Book".into())),
                validate: Arc::new(|_, _| panic!("leaks must be rejected before validation")),
            }),
            ..Default::default()
        };
        let (units, stats) = run_index_units(
            &[tagged],
            true,
            tuning,
            [
                (tagged, false, true, r#"<a i="1">本〖约束要求〗</a>"#),
                (tagged, true, true, "書籍"),
                ("Book", false, false, "本 只输出源文的译文。"),
                ("Book", true, false, "書籍 只输出源文的译文。"),
            ],
        );
        assert_eq!(units, [UnitResult::PromptLeak]);
        assert_eq!((stats.prompt_leaks, stats.retries), (3, 2));
    }

    #[test]
    fn index_accepts_a_quoted_prompt_string_up_to_its_source_count() {
        let source = "Book quotes 〖源文〗.";
        let translated = "本は「〖源文〗」を引用した。";
        let (units, stats) = run_index_units(
            &[source],
            false,
            UnitTuning::default(),
            [
                (
                    source,
                    false,
                    false,
                    "〖源文〗 本は「〖源文〗」を引用した。",
                ),
                (source, true, false, translated),
            ],
        );
        assert_eq!(
            units,
            [UnitResult::Complete {
                text: translated.into(),
                retried: true
            }]
        );
        assert_eq!(stats.prompt_leaks, 1);
    }

    #[test]
    fn index_caches_only_clean_file_units() {
        let (units, stats) = run_index_units(
            &["Book"; 3],
            false,
            UnitTuning {
                reuse_translations: true,
                ..Default::default()
            },
            [
                ("Book", false, false, "本〖硬性要求〗"),
                ("Book", true, false, "本〖硬性要求〗"),
                ("Book", false, false, "本"),
            ],
        );
        let clean = UnitResult::Complete {
            text: "本".into(),
            retried: false,
        };
        assert_eq!(units, [UnitResult::PromptLeak, clean.clone(), clean]);
        assert_eq!(stats.prompt_leaks, 2);
    }

    #[test]
    fn index_txt_omits_history_while_pasted_text_keeps_it() {
        let document = DocumentOptions {
            max_chunk_tokens: 4,
            ..Default::default()
        };
        for (file, history_lengths) in [(true, [0, 0]), (false, [0, 1])] {
            let mut backend = IndexReplies {
                replies: [
                    ("Book", false, false, "本です"),
                    ("Rose", false, false, "バラです"),
                ]
                .into(),
                ..Default::default()
            };
            let translate = if file {
                translate_file_text
            } else {
                translate_document
            };
            translate(
                &mut backend,
                &chars,
                8192,
                &document,
                &index_options(),
                "Book\n\nRose",
                &mut |_| {},
                &mut |_| {},
                &|| false,
            )
            .unwrap();
            assert_eq!(backend.history_lengths, history_lengths);
        }
    }

    #[test]
    fn index_retains_a_leaked_txt_chunk_but_does_not_filter_pasted_text() {
        let leaked = "本〖源文〗";
        for file in [true, false] {
            let mut replies = VecDeque::from([("Book", false, false, leaked)]);
            if file {
                replies.push_back(("Book", true, false, leaked));
            }
            let mut output = String::new();
            let mut cuts = Vec::new();
            let document = DocumentOptions {
                cut_marker: "…".into(),
                ..Default::default()
            };
            let translate = if file {
                translate_file_text
            } else {
                translate_document
            };
            let stats = translate(
                &mut IndexReplies {
                    replies,
                    ..Default::default()
                },
                &chars,
                8192,
                &document,
                &index_options(),
                "Book",
                &mut |event| match event {
                    Output::Text(piece) => output.push_str(piece),
                    Output::Truncate(len) => output.truncate(len),
                },
                &mut |event| {
                    if let Progress::Cut { source, output, .. } = event {
                        cuts.push((source, output));
                    }
                },
                &|| false,
            )
            .unwrap();
            if file {
                assert_eq!(output, "…");
                assert_eq!(cuts, [((0, 4), (0, "…".len()))]);
                assert_eq!((stats.prompt_leaks, stats.retries, stats.cut), (2, 1, 1));
            } else {
                assert_eq!(output, leaked);
                assert!(cuts.is_empty());
                assert_eq!(stats.prompt_leaks, 0);
            }
        }
    }

    struct StreamingHyMt2<'a> {
        events: &'a RefCell<Vec<String>>,
        leak: bool,
    }

    impl TranslationBackend for StreamingHyMt2<'_> {
        fn profile(&self) -> ModelProfile {
            ModelProfile::HyMt2
        }

        fn translate(
            &mut self,
            _: &Prompt<'_>,
            generation: &Generation,
            sink: &mut dyn FnMut(&str),
            _: &dyn Fn() -> bool,
        ) -> Result<TranslateStats> {
            let (pieces, start) = if generation.penalize_repetition {
                assert!(self.leak);
                assert_eq!(
                    *self.events.borrow(),
                    ["  ", "[Source", " Text]", "rewind 2"]
                );
                (["Das", " klappt."], 4)
            } else if self.leak {
                (["[Source", " Text]"], 1)
            } else {
                (["Das", " klappt."], 1)
            };
            for (i, piece) in pieces.into_iter().enumerate() {
                sink(piece);
                // Each event must already be visible while the backend is still generating.
                let events = self.events.borrow();
                assert_eq!(events.len(), start + i + 1);
                assert_eq!(events.last().unwrap(), piece);
            }
            Ok(TranslateStats {
                prompt_tokens: 9,
                generated_tokens: 2,
                generation_duration: Duration::from_millis(200),
                stop_reason: StopReason::Finished,
                ..TranslateStats::cancelled()
            })
        }
    }

    fn stream_hy_mt2_text(leak: bool) -> (Vec<String>, TranslateStats) {
        let events = RefCell::new(Vec::new());
        let mut backend = StreamingHyMt2 {
            events: &events,
            leak,
        };
        let mut options = TranslationOptions::new(pfml::LangCode::new("de").unwrap());
        options.source = Some(pfml::LangCode::new("en").unwrap());
        let stats = translate_document(
            &mut backend,
            &str::len,
            8192,
            &DocumentOptions::default(),
            &options,
            "  Sample",
            &mut |event| {
                events.borrow_mut().push(match event {
                    Output::Text(text) => text.into(),
                    Output::Truncate(bytes) => format!("rewind {bytes}"),
                });
            },
            &mut |_| {},
            &|| false,
        )
        .unwrap();
        (events.into_inner(), stats)
    }

    #[test]
    fn hy_mt2_text_streams_each_piece_and_preserves_token_and_speed_counts() {
        let (events, stats) = stream_hy_mt2_text(false);
        assert_eq!(events, ["  ", "Das", " klappt."]);
        assert_eq!(stats.prompt_tokens, 9);
        assert_eq!(stats.generated_tokens, 2);
        assert!((stats.generation_tokens_per_second() - 10.0).abs() < f64::EPSILON);
        assert_eq!(stats.retries, 0);
    }

    #[test]
    fn hy_mt2_text_rewinds_the_leaked_attempt_before_streaming_the_retry() {
        let (events, stats) = stream_hy_mt2_text(true);
        assert_eq!(
            events,
            ["  ", "[Source", " Text]", "rewind 2", "Das", " klappt."]
        );
        assert_eq!(stats.prompt_tokens, 18);
        assert_eq!(stats.generated_tokens, 4);
        assert_eq!(stats.retries, 1);
        assert_eq!(stats.prompt_leaks, 1);
    }

    #[derive(Clone, Copy, PartialEq, Eq)]
    enum LeakPosition {
        None,
        Leading,
        Later,
    }

    struct RecoveringHyMt2<'a> {
        output: &'a RefCell<String>,
        leak: LeakPosition,
        whole_attempts: usize,
        leak_second_sentence: bool,
    }

    const RECOVERY_SOURCE: &str = "One. Two. Three.";

    impl TranslationBackend for RecoveringHyMt2<'_> {
        fn profile(&self) -> ModelProfile {
            ModelProfile::HyMt2
        }

        fn is_hy_mt2_1_8b(&self) -> bool {
            true
        }

        fn translate(
            &mut self,
            prompt: &Prompt<'_>,
            generation: &Generation,
            sink: &mut dyn FnMut(&str),
            is_cancelled: &dyn Fn() -> bool,
        ) -> Result<TranslateStats> {
            assert!(prompt.history.is_empty());
            if prompt.text == RECOVERY_SOURCE {
                self.whole_attempts += 1;
                assert_eq!(generation.penalize_repetition, self.whole_attempts == 2);
            }
            match prompt.text {
                RECOVERY_SOURCE if self.leak == LeakPosition::None => {
                    sink("一。");
                    assert_eq!(self.output.borrow().as_str(), "一。");
                    sink(" 二。 三。");
                }
                "One." => sink("一。"),
                "Two." if !self.leak_second_sentence => sink("二。"),
                "Three." => sink("三。"),
                _ => {
                    let before = self.output.borrow().len();
                    if self.leak == LeakPosition::Later {
                        sink("候補。");
                        assert!(self.output.borrow().ends_with("候補。"));
                    }
                    sink("\n[Source");
                    assert!(!is_cancelled());
                    sink(" Text]");
                    if self.leak == LeakPosition::Leading {
                        assert_eq!(self.output.borrow().len(), before);
                        assert!(is_cancelled());
                        return Ok(TranslateStats {
                            generated_tokens: 2,
                            ..TranslateStats::cancelled()
                        });
                    }
                    assert!(!is_cancelled());
                    sink(" retained only until the full check");
                }
            }
            Ok(TranslateStats {
                generated_tokens: 2,
                stop_reason: StopReason::Finished,
                ..TranslateStats::cancelled()
            })
        }
    }

    fn translate_test_candidate(
        leak: LeakPosition,
        leak_second_sentence: bool,
    ) -> (String, TranslateStats) {
        let output = RefCell::new(String::new());
        let mut backend = RecoveringHyMt2 {
            output: &output,
            leak,
            whole_attempts: 0,
            leak_second_sentence,
        };
        let mut options = TranslationOptions::new(pfml::LangCode::new("ja").unwrap());
        options.source = Some(pfml::LangCode::new("en").unwrap());
        let document = DocumentOptions {
            cut_marker: "…".into(),
            ..DocumentOptions::default()
        };
        let stats = translate_document(
            &mut backend,
            &str::len,
            8192,
            &document,
            &options,
            RECOVERY_SOURCE,
            &mut |event| match event {
                Output::Text(text) => {
                    assert!(!text.contains("[Source") && !text.contains(" Text]"));
                    output.borrow_mut().push_str(text);
                }
                Output::Truncate(bytes) => {
                    assert!(leak == LeakPosition::Later);
                    output.borrow_mut().truncate(bytes);
                }
            },
            &mut |_| {},
            &|| false,
        )
        .unwrap();
        assert_eq!(
            backend.whole_attempts,
            if leak == LeakPosition::Later { 2 } else { 1 }
        );
        (output.into_inner(), stats)
    }

    #[test]
    fn hy_mt2_1_8b_stops_at_a_leading_header_before_emitting_candidate_text() {
        let (output, stats) = translate_test_candidate(LeakPosition::Leading, false);
        assert_eq!(output, "一。 二。 三。");
        assert_eq!(stats.prompt_leaks, 1);
        assert_eq!(stats.retries, 1);
        assert_eq!(stats.cut, 0);
        assert_eq!(stats.stop_reason, StopReason::Finished);
    }

    #[test]
    fn hy_mt2_1_8b_streams_a_clean_candidate_before_it_finishes() {
        let (output, stats) = translate_test_candidate(LeakPosition::None, false);
        assert_eq!(output, "一。 二。 三。");
        assert_eq!(stats.prompt_leaks, 0);
        assert_eq!(stats.retries, 0);
    }

    #[test]
    fn hy_mt2_1_8b_translates_sentences_after_two_rejected_candidates() {
        let (output, stats) = translate_test_candidate(LeakPosition::Later, false);
        assert_eq!(output, "一。 二。 三。");
        assert_eq!(stats.prompt_leaks, 2);
        assert_eq!(stats.retries, 2);
        assert_eq!(stats.cut, 0);
    }

    #[test]
    fn hy_mt2_1_8b_cuts_only_the_sentence_that_still_leaks() {
        let (output, stats) = translate_test_candidate(LeakPosition::Later, true);
        assert_eq!(output, "一。 … 三。");
        assert_eq!(stats.prompt_leaks, 4);
        assert_eq!(stats.cut, 1);
    }

    fn rejoin(split: &Split<'_>) -> String {
        let mut out = split.leading.to_owned();
        for c in &split.chunks {
            out.push_str(c.text);
            out.push_str(c.separator);
        }
        out
    }

    #[test]
    fn short_text_is_one_chunk() {
        let s = split("Hello.\n\nWorld.\n", 100, &chars);
        assert_eq!(
            s.chunks,
            vec![Chunk {
                text: "Hello.\n\nWorld.",
                separator: "\n"
            }]
        );
    }

    #[test]
    fn splits_paragraphs_and_keeps_layout() {
        let text = "  First one here.\n\n\nSecond one.\n  \nThird.";
        let s = split(text, 15, &chars);
        assert_eq!(s.leading, "  ");
        let texts: Vec<_> = s.chunks.iter().map(|c| c.text).collect();
        assert_eq!(texts, ["First one here.", "Second one.", "Third."]);
        assert_eq!(rejoin(&s), text);
    }

    #[test]
    fn packs_small_paragraphs() {
        let s = split("a.\n\nb.\n\nc.\n\nd.", 7, &chars);
        let texts: Vec<_> = s.chunks.iter().map(|c| c.text).collect();
        assert_eq!(texts, ["a.\n\nb.", "c.\n\nd."]);
    }

    #[test]
    fn rewinds_a_packed_translation_with_missing_separators() {
        #[derive(Default)]
        struct Joined {
            prompts: Vec<(String, Vec<String>)>,
        }

        impl TranslationBackend for Joined {
            fn translate(
                &mut self,
                prompt: &Prompt<'_>,
                _: &Generation,
                sink: &mut dyn FnMut(&str),
                _: &dyn Fn() -> bool,
            ) -> Result<TranslateStats> {
                self.prompts.push((
                    prompt.text.to_owned(),
                    prompt
                        .history
                        .iter()
                        .map(|pair| pair.translation.clone())
                        .collect(),
                ));
                let output = if prompt.text.contains('\n') {
                    "Merged candidate.".to_owned()
                } else {
                    prompt.text.to_uppercase()
                };
                sink(&output);
                Ok(stats(StopReason::Finished, &output))
            }
        }

        for separator in ["\r\n \r\n", "\r\n"] {
            let text = format!("  First sentence.{separator}Second sentence.");
            let mut backend = Joined::default();
            let mut visible = String::new();
            let mut snapshots = Vec::new();
            let mut progress = Vec::new();
            let result = translate_document(
                &mut backend,
                &chars,
                100_000,
                &DocumentOptions::default(),
                &TranslationOptions::new(pfml::LangCode::new("en").unwrap()),
                &text,
                &mut |event| {
                    match event {
                        Output::Text(value) => visible.push_str(value),
                        Output::Truncate(len) => visible.truncate(len),
                    }
                    snapshots.push(visible.clone());
                },
                &mut |event| progress.push(event),
                &|| false,
            )
            .unwrap();
            assert_eq!(result.retries, 1);
            assert_eq!(result.prompt_tokens, 3);
            assert!(matches!(
                progress.as_slice(),
                [
                    Progress::Chunk { count: 1, .. },
                    Progress::ParagraphFallback {
                        index: 0,
                        paragraphs: 2
                    }
                ]
            ));
            assert_eq!(
                backend.prompts,
                [
                    (text.trim_start().to_owned(), vec![]),
                    ("First sentence.".to_owned(), vec![]),
                    (
                        "Second sentence.".to_owned(),
                        vec!["FIRST SENTENCE.".to_owned()]
                    ),
                ],
            );
            assert_eq!(
                snapshots,
                [
                    "  ".to_owned(),
                    "  Merged candidate.".to_owned(),
                    "  ".to_owned(),
                    "  FIRST SENTENCE.".to_owned(),
                    format!("  FIRST SENTENCE.{separator}"),
                    text.to_uppercase(),
                ],
            );
        }
    }

    struct Reply<'a> {
        source: &'a str,
        output: &'a [&'a str],
        reason: StopReason,
        penalized: bool,
    }

    fn reply<'a>(source: &'a str, output: &'a [&'a str]) -> Reply<'a> {
        Reply {
            source,
            output,
            reason: StopReason::Finished,
            penalized: false,
        }
    }

    struct ParagraphBackend<'a> {
        replies: std::collections::VecDeque<Reply<'a>>,
        hy_1_8b: bool,
        cancelled: &'a Cell<bool>,
    }

    impl TranslationBackend for ParagraphBackend<'_> {
        fn profile(&self) -> ModelProfile {
            if self.hy_1_8b {
                ModelProfile::HyMt2
            } else {
                ModelProfile::PlamoPfml
            }
        }

        fn is_hy_mt2_1_8b(&self) -> bool {
            self.hy_1_8b
        }

        fn translate(
            &mut self,
            prompt: &Prompt<'_>,
            generation: &Generation,
            sink: &mut dyn FnMut(&str),
            is_cancelled: &dyn Fn() -> bool,
        ) -> Result<TranslateStats> {
            let next = self
                .replies
                .pop_front()
                .expect("unexpected translation attempt");
            assert_eq!(prompt.text, next.source);
            assert_eq!(generation.penalize_repetition, next.penalized);
            if self.hy_1_8b {
                assert!(prompt.history.is_empty());
            }
            assert!(!is_cancelled());
            for piece in next.output {
                sink(piece);
            }
            if next.reason == StopReason::Cancelled {
                if next.output == ["\n[Source", " Text]"] {
                    assert!(is_cancelled(), "the leading header should stop generation");
                    assert!(!self.cancelled.get());
                } else {
                    self.cancelled.set(true);
                    assert!(is_cancelled());
                }
            }
            Ok(stats(next.reason, &next.output.concat()))
        }
    }

    fn run_paragraph_script<'a>(
        text: &str,
        replies: impl IntoIterator<Item = Reply<'a>>,
        hy_1_8b: bool,
    ) -> (String, TranslateStats, Vec<usize>) {
        let cancelled = Cell::new(false);
        let mut backend = ParagraphBackend {
            replies: replies.into_iter().collect(),
            hy_1_8b,
            cancelled: &cancelled,
        };
        let mut out = String::new();
        let mut rewinds = Vec::new();
        let mut options = TranslationOptions::new(pfml::LangCode::new("ja").unwrap());
        options.source = Some(pfml::LangCode::new("en").unwrap());
        let document = DocumentOptions {
            cut_marker: "…".into(),
            ..DocumentOptions::default()
        };
        let stats = translate_document(
            &mut backend,
            &|text| text.chars().count().div_ceil(4),
            100_000,
            &document,
            &options,
            text,
            &mut |event| match event {
                Output::Text(text) => {
                    assert!(!text.contains("[Source") && !text.contains(" Text]"));
                    out.push_str(text);
                }
                Output::Truncate(bytes) => {
                    assert!(out.is_char_boundary(bytes));
                    rewinds.push(bytes);
                    out.truncate(bytes);
                }
            },
            &mut |_| {},
            &|| cancelled.get(),
        )
        .unwrap();
        assert!(backend.replies.is_empty());
        (out, stats, rewinds)
    }

    #[test]
    fn accepts_unwrapped_translations_and_equivalent_blank_line_runs() {
        for separator in ["\n", "\n\n\n"] {
            let source = format!("{WRAPPED_EN}{separator}{WRAPPED_EN}");
            let prompt = format!("{UNWRAPPED_EN}{separator}{UNWRAPPED_EN}");
            let output = if separator == "\n" {
                "一段落目です。\n二段落目です。"
            } else {
                "一段落目です。\n\n二段落目です。"
            };
            let (out, stats, rewinds) =
                run_paragraph_script(&source, [reply(&prompt, &[output])], false);
            assert_eq!(out, output);
            assert_eq!(stats.retries, 0);
            assert!(rewinds.is_empty());
        }
    }

    #[test]
    fn ignores_single_newlines_when_unjoined_wraps_remain() {
        let source = "First sentence.\nNext sentence\nwrapped here.\n\nSecond paragraph.";
        let output = "一段落目です。続きの文です。\n\n二段落目です。";
        let (out, stats, rewinds) = run_paragraph_script(source, [reply(source, &[output])], false);
        assert_eq!(out, output);
        assert_eq!(stats.retries, 0);
        assert!(rewinds.is_empty());
    }

    #[test]
    fn normalizes_streamed_cjk_and_latin_wraps() {
        let (out, stats, rewinds) = run_paragraph_script(
            WRAPPED_EN,
            [reply(
                UNWRAPPED_EN,
                &[
                    "提灯。",
                    "\r",
                    "\n ",
                    "教室の ",
                    "\n",
                    "話！",
                    "\n",
                    "（午前）３",
                    "\n",
                    "人。",
                    "\n",
                    "Lantern",
                    "\n",
                    "class.",
                ],
            )],
            false,
        );
        assert_eq!(out, "提灯。教室の話！（午前）３人。 Lantern class.");
        assert_eq!(stats.retries, 0);
        assert!(rewinds.is_empty());
        assert_eq!(flatten_breaks("前\n\n後\n行", 2), "前後\n行");
    }

    #[test]
    fn hy_mt2_1_8b_recovers_a_leading_header_during_paragraph_retry() {
        let first = "First. Second. Third.";
        let source = "First. Second. Third.\n\nFourth.";
        let (out, stats, rewinds) = run_paragraph_script(
            source,
            [
                reply(source, &["連結。"]),
                Reply {
                    reason: StopReason::Cancelled,
                    ..reply(first, &["\n[Source", " Text]"])
                },
                reply("First.", &["一。"]),
                reply("Second.", &["二。"]),
                reply("Third.", &["三。"]),
                reply("Fourth.", &["四。"]),
            ],
            true,
        );
        assert_eq!(out, "一。 二。 三。\n\n四。");
        assert_eq!(stats.stop_reason, StopReason::Finished);
        assert_eq!((stats.retries, stats.prompt_leaks, stats.cut), (2, 1, 0));
        assert_eq!(rewinds, [0]);
    }

    #[test]
    fn hy_mt2_1_8b_streams_the_clean_paragraph_after_an_internal_prompt_leak() {
        let source = "First.\n\nSecond.";
        let (out, stats, rewinds) = run_paragraph_script(
            source,
            [
                reply(source, &["連結。"]),
                reply("First.", &["候補。", "\n[Source", " Text]", " leaked"]),
                Reply {
                    penalized: true,
                    ..reply("First.", &["最初の", "\n", "段落。"])
                },
                reply("Second.", &["二。"]),
            ],
            true,
        );
        assert_eq!(out, "最初の段落。\n\n二。");
        assert_eq!((stats.retries, stats.prompt_leaks, stats.cut), (2, 1, 0));
        assert_eq!(rewinds, [0, 0]);
    }

    #[test]
    fn cancelling_the_paragraph_retry_keeps_the_partial_output() {
        let source = "First.\n\nSecond.";
        let (out, stats, rewinds) = run_paragraph_script(
            source,
            [
                reply(source, &["連結。"]),
                Reply {
                    reason: StopReason::Cancelled,
                    ..reply("First.", &["最初の", "\n", "段"])
                },
            ],
            true,
        );
        assert_eq!(out, "最初の段");
        assert_eq!(stats.stop_reason, StopReason::Cancelled);
        assert_eq!(stats.retries, 1);
        assert_eq!(rewinds, [0]);
    }

    #[test]
    fn cancellation_discards_pending_whitespace_from_a_leaked_paragraph() {
        let source = "First.\n\nSecond.";
        let (out, stats, rewinds) = run_paragraph_script(
            source,
            [
                reply(source, &["連結。"]),
                Reply {
                    reason: StopReason::Cancelled,
                    ..reply("First.", &["候補。 ", "\n[Source", " Text]", " leaked"])
                },
            ],
            true,
        );
        assert!(out.is_empty());
        assert_eq!(stats.stop_reason, StopReason::Cancelled);
        assert_eq!((stats.retries, stats.prompt_leaks), (1, 1));
        assert_eq!(rewinds, [0, 0]);
    }

    #[test]
    fn runaway_during_paragraph_retry_cuts_at_the_normalized_byte_offset() {
        let source = "First.\n\nSecond.";
        let output = &["漢", "\r\n", "字漢字漢字"];
        let reason = StopReason::Repetition {
            kept: "漢\r\n字".len(),
        };
        let (out, stats, rewinds) = run_paragraph_script(
            source,
            [
                reply(source, &["連結。"]),
                Reply {
                    reason,
                    ..reply("First.", output)
                },
                Reply {
                    reason,
                    penalized: true,
                    ..reply("First.", output)
                },
                reply("Second.", &["次。"]),
            ],
            false,
        );
        assert_eq!(out, "漢字…\n\n次。");
        assert_eq!((stats.retries, stats.cut), (2, 1));
        assert_eq!(rewinds, [0, 0, "漢字".len()]);
    }

    #[test]
    fn splits_long_paragraphs_at_sentences() {
        let text = "これは一文目です。これは二文目です。「三文目！」最後の文。";
        let s = split(text, 10, &chars);
        let texts: Vec<_> = s.chunks.iter().map(|c| c.text).collect();
        assert_eq!(
            texts,
            [
                "これは一文目です。",
                "これは二文目です。",
                "「三文目！」",
                "最後の文。"
            ]
        );
        assert_eq!(rejoin(&s), text);

        let text = "One sentence. Two sentences! Version 1.5 is out? Yes.";
        let s = split(text, 20, &chars);
        let texts: Vec<_> = s.chunks.iter().map(|c| c.text).collect();
        assert_eq!(
            texts,
            [
                "One sentence.",
                "Two sentences!",
                "Version 1.5 is out?",
                "Yes."
            ]
        );
        assert_eq!(rejoin(&s), text);
    }

    /// Stats for a fake generation of `output`, one token per character.
    fn stats(stop_reason: StopReason, output: &str) -> TranslateStats {
        TranslateStats {
            prompt_tokens: 1,
            generated_tokens: output.chars().count(),
            prompt_duration: Duration::ZERO,
            generation_duration: Duration::ZERO,
            stop_reason,
            retries: 0,
            cut: 0,
            prompt_leaks: 0,
        }
    }

    /// Uppercases its input and records the prompts it received. Inputs containing "LOOP" loop
    /// when decoded greedily, and also with repetition penalized if `stubborn`. A multi-sentence
    /// input containing "SHORT" is answered with just "A".
    struct Upper {
        prompts: Vec<String>,
        stubborn: bool,
    }

    impl Upper {
        fn new() -> Self {
            Self {
                prompts: vec![],
                stubborn: false,
            }
        }
    }

    impl TranslationBackend for Upper {
        fn translate(
            &mut self,
            prompt: &Prompt<'_>,
            generation: &Generation,
            sink: &mut dyn FnMut(&str),
            _: &dyn Fn() -> bool,
        ) -> Result<TranslateStats> {
            self.prompts.push(prompt.render()?);
            if prompt.text.contains("LOOP") && (self.stubborn || !generation.penalize_repetition) {
                sink("la la la la");
                return Ok(stats(StopReason::Repetition { kept: 3 }, "la la la la"));
            }
            if prompt.text.contains("SHORT") && prompt.text.contains('b') {
                sink("A");
                return Ok(stats(StopReason::Finished, "A"));
            }
            let output = prompt.text.to_uppercase();
            sink(&output);
            Ok(stats(StopReason::Finished, &output))
        }
    }

    /// Runs [`translate_document`] and applies its output, returning the text, the stats, and
    /// the progress reports.
    fn run(
        backend: &mut Upper,
        count_tokens: &dyn Fn(&str) -> usize,
        document: DocumentOptions,
        text: &str,
    ) -> (String, TranslateStats, Vec<Progress>) {
        let options = TranslationOptions::new(pfml::LangCode::new("en").unwrap());
        let mut out = String::new();
        let mut progress = vec![];
        let stats = translate_document(
            backend,
            count_tokens,
            100_000,
            &document,
            &options,
            text,
            &mut |o| match o {
                Output::Text(s) => out.push_str(s),
                Output::Truncate(len) => out.truncate(len),
            },
            &mut |p| progress.push(p),
            &|| false,
        )
        .unwrap();
        (out, stats, progress)
    }

    #[test]
    fn translates_chunks_with_history() {
        let mut backend = Upper::new();
        // 3 words = 180 tokens: with chunks of at most 200 tokens the two paragraphs are
        // separate, and 500 tokens of history hold the first pair.
        let document = DocumentOptions {
            max_chunk_tokens: 200,
            max_history_tokens: 500,
            ..DocumentOptions::default()
        };
        let text = "\nalpha beta gamma.\n\ndelta epsilon zeta.\n";
        let words = |s: &str| s.split_whitespace().count() * 60;
        let (out, stats, progress) = run(&mut backend, &words, document, text);
        assert_eq!(out, "\nALPHA BETA GAMMA.\n\nDELTA EPSILON ZETA.\n");
        assert_eq!(
            progress,
            [
                Progress::Chunk {
                    index: 0,
                    count: 2,
                    source: (1, 18)
                },
                Progress::Chunk {
                    index: 1,
                    count: 2,
                    source: (20, 39)
                }
            ]
        );
        assert_eq!(stats.prompt_tokens, 2);
        assert_eq!((stats.retries, stats.cut), (0, 0));
        // The second prompt carries the first pair as history.
        assert!(backend.prompts[1].contains("alpha beta gamma.<|plamo:tag|>output"));
        assert!(backend.prompts[1].contains("<|plamo:msg|>ALPHA BETA GAMMA."));
    }

    #[test]
    fn retries_a_runaway_chunk_sentence_by_sentence() {
        let mut backend = Upper::new();
        let document = DocumentOptions {
            max_chunk_tokens: 25,
            max_history_tokens: 1000,
            ..DocumentOptions::default()
        };
        let text = "Good one. Bad LOOP one.\n\nNext.";
        let (out, stats, progress) = run(&mut backend, &chars, document, text);
        assert_eq!(out, "GOOD ONE. BAD LOOP ONE.\n\nNEXT.");
        // The chunk, then its second sentence.
        assert_eq!((stats.retries, stats.cut), (2, 0));
        assert_eq!(
            progress[1],
            Progress::Retry {
                index: 0,
                reason: StopReason::Repetition { kept: 3 },
                pieces: 2
            }
        );
        // Prompts: chunk, sentence 1, sentence 2, sentence 2 penalized, next chunk.
        assert_eq!(backend.prompts.len(), 5);
        // Output that looped never becomes history.
        assert!(backend.prompts.iter().all(|p| !p.contains("la la")));
        assert!(backend.prompts[4].contains("<|plamo:msg|>BAD LOOP ONE."));
    }

    #[test]
    fn retries_a_chunk_that_ended_early() {
        let mut backend = Upper::new();
        let text = format!("{} SHORT. {}.", "a".repeat(150), "b".repeat(100));
        let (out, stats, progress) = run(&mut backend, &chars, DocumentOptions::default(), &text);
        assert_eq!(out, text.to_uppercase());
        assert_eq!((stats.retries, stats.cut), (1, 0));
        assert_eq!(
            progress[1],
            Progress::Retry {
                index: 0,
                reason: StopReason::Incomplete,
                pieces: 2
            }
        );
        // A short source may well have a short translation.
        assert!(!looks_incomplete(150, 10));
        assert!(!looks_incomplete(400, 100));
        assert!(looks_incomplete(400, 99));
    }

    #[test]
    fn retries_in_pieces_then_sentences() {
        let mut backend = Upper::new();
        let text = format!(
            "{}.\n\n{}. LOOP {}.\n\n{}.",
            "a".repeat(200),
            "b".repeat(100),
            "c".repeat(80),
            "d".repeat(200)
        );
        let (out, stats, progress) = run(&mut backend, &chars, DocumentOptions::default(), &text);
        assert_eq!(out, text.to_uppercase());
        // The chunk, its second piece, and that piece's second sentence.
        assert_eq!((stats.retries, stats.cut), (3, 0));
        let retries: Vec<_> = progress
            .iter()
            .filter_map(|p| match p {
                Progress::Retry { pieces, .. } => Some(*pieces),
                _ => None,
            })
            .collect();
        assert_eq!(retries, [3, 2]);
        // Chunk, piece 1, piece 2, its sentences 1 and 2, sentence 2 penalized, piece 3.
        assert_eq!(backend.prompts.len(), 7);
    }

    #[test]
    fn marks_where_output_was_cut() {
        let mut backend = Upper {
            prompts: vec![],
            stubborn: true,
        };
        let document = DocumentOptions {
            max_chunk_tokens: 6,
            max_history_tokens: 1000,
            cut_marker: "[CUT]".into(),
        };
        let (out, _, progress) = run(&mut backend, &chars, document, "LOOP.\n\nNext.");
        assert_eq!(out, "la [CUT]\n\nNEXT.");
        // The reported output range covers the marker.
        assert!(
            progress
                .iter()
                .any(|p| matches!(p, Progress::Cut { output: (0, 8), .. }))
        );
    }

    #[test]
    fn translates_segments_in_order_with_history() {
        let mut backend = Upper::new();
        let options = TranslationOptions::new(pfml::LangCode::new("en").unwrap());
        let segments = vec![
            "first one.".to_owned(),
            "".to_owned(),
            "LOOP two.".to_owned(),
        ];
        let mut results = vec![];
        let stats = translate_segments(
            &mut backend,
            &chars,
            100_000,
            &DocumentOptions::default(),
            &options,
            &segments,
            &mut |i, text| results.push((i, text)),
            &|| false,
        )
        .unwrap();
        assert_eq!(
            results,
            [
                (0, "FIRST ONE.".to_owned()),
                (1, String::new()),
                (2, "LOOP TWO.".to_owned())
            ]
        );
        // The looping segment was retried; the third prompt has the first pair as history.
        assert_eq!(stats.retries, 1);
        assert!(backend.prompts[1].contains("<|plamo:msg|>FIRST ONE."));
    }

    #[test]
    fn cuts_output_that_keeps_looping() {
        let mut backend = Upper {
            prompts: vec![],
            stubborn: true,
        };
        // Chunks of one paragraph each.
        let document = DocumentOptions {
            max_chunk_tokens: 6,
            max_history_tokens: 1000,
            ..DocumentOptions::default()
        };
        let (out, stats, progress) = run(&mut backend, &chars, document, "LOOP.\n\nNext.");
        // The single sentence goes straight to the penalized retry, and its first cycle is kept.
        assert_eq!(out, "la \n\nNEXT.");
        assert_eq!((stats.retries, stats.cut), (1, 1));
        assert_eq!(backend.prompts.len(), 3);
        assert!(progress.contains(&Progress::Cut {
            index: 0,
            reason: StopReason::Repetition { kept: 3 },
            source: (0, 5),
            output: (0, 3),
        }));
    }

    #[test]
    fn history_keeps_the_most_recent_pairs() {
        let pair = |s: &str| HistoryPair {
            source: s.into(),
            translation: s.into(),
        };
        let history = [pair("aaaa"), pair("bb"), pair("c")];
        // "c" costs 18 and "bb" 20, so a budget of 40 keeps exactly those two.
        let recent = recent_history(&history, &[24, 20, 18], 40);
        assert_eq!(recent.len(), 2);
        assert_eq!(recent[0].source, "bb");
    }

    const WRAPPED_EN: &str = "The volunteers counted the lantern frames before noon and left\n\
        some in the shade. The organizers set out paper for each team.\n\
        They left the spare rolls beside the door for the next group\n\
        to use.";
    const UNWRAPPED_EN: &str = "The volunteers counted the lantern frames before noon and left \
        some in the shade. The organizers set out paper for each team. \
        They left the spare rolls beside the door for the next group \
        to use.";
    const WRAPPED_JA: &str = "参加者が迷わず展示会場に着けるように入口で案内係が\n\
        会場の地図と当日の資料を一組ずつそろえて参加者に\n\
        先に渡しました。案内が終わったら次の組のために机\n\
        を片付けます。";
    const RAGGED_EN: &str = "The volunteers counted the lantern frames before noon and left\n\
        them beside the door with a note that invited\n\
        anyone who still needed one when the organizers began the class\n\
        in the large room at the far side of the building that morning.\n\
        They left the spare rolls beside the door for the next group\n\
        to use.";

    #[test]
    fn joins_full_width_latin_lines_even_after_sentence_punctuation() {
        assert_eq!(Reflowed::new(WRAPPED_EN).text, UNWRAPPED_EN);
    }

    #[test]
    fn leaves_visible_hyphens_in_wrapped_compounds() {
        for (prefix, suffix) in [("well-", "known"), ("zero-", "shot"), ("illus-", "trated")] {
            let wrapped = format!(
                "The volunteers counted the lantern frames and used the {prefix}\n\
                {suffix} guides that the organizers left there for each team.\n\
                They left the spare rolls beside the door for the next group\n\
                to use."
            );
            assert_eq!(
                Reflowed::new(&wrapped).text,
                wrapped
                    .replace(&format!("{prefix}\n"), prefix)
                    .replace('\n', " ")
            );
        }
    }

    #[test]
    fn removes_discretionary_soft_hyphens_when_joining() {
        let wrapped = "The volunteers counted the lantern frames and used the illus\u{ad}\n\
            trated guides that the organizers left there for each team.\n\
            They left the spare rolls beside the door for the next group\n\
            to use.";
        assert_eq!(
            Reflowed::new(wrapped).text,
            wrapped.replace("\u{ad}\n", "").replace('\n', " ")
        );
    }

    #[test]
    fn joins_fixed_width_japanese_wraps_without_spaces() {
        assert_eq!(Reflowed::new(WRAPPED_JA).text, WRAPPED_JA.replace('\n', ""));
    }

    #[test]
    fn joins_japanese_wraps_next_to_full_width_forms() {
        let wrapped = "参加者が迷わず展示会場に着けるように入口で案内係が\n\
            会場の地図と当日の資料を一組ずつ先に渡しました！\n\
            （案内が終わったら次の組のために会場の机を片付け、\n\
            参加者席を戻します。）";
        assert_eq!(Reflowed::new(wrapped).text, wrapped.replace('\n', ""));
        for form in ['！', '？', '（', '）', '，', '：', '３', '･'] {
            assert_eq!(word_separator(form, '次'), "");
            assert_eq!(word_separator('次', form), "");
        }
    }

    #[test]
    fn joins_first_line_indents_into_their_paragraphs() {
        let short_ja = "参加者が迷わず展示会場に着けるように入口で案内係が\n\
            会場の地図を渡しました。";
        let short_en = "The volunteers counted the lantern frames before noon and left\n\
            some in the shade.";
        for (indent, wrapped, expected) in [
            ("\u{3000}", WRAPPED_JA, WRAPPED_JA.replace('\n', "")),
            ("\u{3000}", short_ja, short_ja.replace('\n', "")),
            ("    ", WRAPPED_EN, UNWRAPPED_EN.to_owned()),
            ("    ", short_en, short_en.replace('\n', " ")),
        ] {
            for separator in ["\n", "\n\n"] {
                let text = format!("{indent}{wrapped}{separator}{indent}{wrapped}");
                let reflowed = Reflowed::new(&text);
                let paragraphs = reflowed.paragraphs(&reflowed.text);
                assert_eq!(paragraphs.len(), 2);
                assert_eq!(
                    reflowed.text,
                    format!("{indent}{expected}{separator}{indent}{expected}")
                );
                for (start, end, _) in paragraphs {
                    assert_eq!(&reflowed.text[start..end], expected);
                }
            }
        }
    }

    #[test]
    fn joins_wrapped_list_items_at_aligned_and_hanging_indents() {
        for (first, second, indent) in [("• ", "• ", ""), ("1. ", "2. ", "   ")] {
            let item = |marker| {
                format!(
                    "{marker}Count the frames at the start of the workshop and set some\n\
                     {indent}aside for anyone who needs one. Then take the spare paper\n\
                     {indent}to the large room."
                )
            };
            let input = format!("{}\n{}", item(first), item(second));
            let reflowed = Reflowed::new(&input);
            let paragraphs = reflowed.paragraphs(&reflowed.text);
            assert_eq!(paragraphs.len(), 2);
            for ((start, end, _), marker) in paragraphs.into_iter().zip([first, second]) {
                assert_eq!(
                    &reflowed.text[start..end],
                    item(marker).replace(&format!("\n{indent}"), " ")
                );
            }
        }
    }

    #[test]
    fn leaves_indeterminate_breaks_in_ragged_prose() {
        assert_eq!(classify_breaks(RAGGED_EN, "")[1].1, LineBreak::Unknown);
        let reflowed = Reflowed::new(RAGGED_EN);
        assert!(reflowed.was_reflowed());
        assert_eq!(reflowed.text.matches('\n').count(), 1);
        assert!(reflowed.text.contains("that invited\nanyone"));
        assert_eq!(reflowed.paragraphs(&reflowed.text).len(), 1);

        let indented = RAGGED_EN.replace("\nanyone", "\n  anyone");
        let reflowed = Reflowed::new(&indented);
        assert!(reflowed.text.contains("that invited\n  anyone"));
        assert_eq!(reflowed.paragraphs(&reflowed.text).len(), 1);
    }

    #[test]
    fn leaves_regular_unpunctuated_lists_and_chat_lines_unjoined() {
        for text in [
            "Spare drawing paper for the tables beside the entrance\n\
             Extra wooden frames for the tables near the back wall\n\
             Fresh pencils and erasers for the afternoon workshop\n\
             Unused paint and brushes for next week's morning class",
            "Can everyone meet at the station before the first train\n\
             I can wait near the ticket counter until you get there\n\
             We have extra time if the shops are still open nearby\n\
             Please send a message when you turn onto Station Road",
            "午前の体験教室で使用する木枠と紙の予備保管場所\n\
             当日の参加受付に必要な名簿と案内図の更新計画\n\
             午後の体験教室で使用する筆と絵の具の回収手順\n\
             来月の展示会に向けた作品と写真の提出期限一覧",
            "午前の体験教室で使う木枠と紙を多めに用意する\n\
             当日の参加受付に必要な名簿と案内図を印刷する\n\
             午後の体験教室で使う筆と絵の具を先に用意する\n\
             来月の展示会に向けて作品と写真の提出を依頼する",
        ] {
            assert!(matches!(Reflowed::new(text).text, Cow::Borrowed(_)));
        }
    }

    #[test]
    fn layout_retry_uses_confident_boundaries_around_an_unknown_break() {
        let input = format!("{RAGGED_EN}\n    {WRAPPED_EN}");
        let ragged = RAGGED_EN.lines().collect::<Vec<_>>();
        let first = format!("{} {}\n{}", ragged[0], ragged[1], ragged[2..].join(" "));
        let prompt = format!("{first}\n    {UNWRAPPED_EN}");
        let kept = "最初の段落です。\n次の段落です。";

        let (out, stats, rewinds) = run_paragraph_script(&input, [reply(&prompt, &[kept])], false);
        assert_eq!(out, kept);
        assert_eq!(stats.retries, 0);
        assert!(rewinds.is_empty());

        let (out, stats, rewinds) = run_paragraph_script(
            &input,
            [
                reply(&prompt, &["連結。"]),
                reply(&first, &["最初の段落です。"]),
                reply(UNWRAPPED_EN, &["次の段落です。"]),
            ],
            false,
        );
        assert_eq!(out, "最初の段落です。\n    次の段落です。");
        assert_eq!(stats.retries, 1);
        assert_eq!(rewinds, [0]);
    }

    #[test]
    fn keeps_original_paragraph_separators_and_source_ranges() {
        let first = WRAPPED_EN.replace('\n', "\r\n");
        let separator = "\r\n \u{3000}\r\n\r\n";
        let second = "The next meeting starts at noon.";
        let text = format!("{first}{separator}{second}\r\n");
        let mut backend = Upper::new();
        let document = DocumentOptions {
            max_chunk_tokens: UNWRAPPED_EN.len(),
            ..DocumentOptions::default()
        };
        let (out, _, progress) = run(&mut backend, &chars, document, &text);
        assert_eq!(
            out,
            format!("{UNWRAPPED_EN}{separator}{second}\r\n").to_uppercase()
        );
        assert_eq!(backend.prompts.len(), 2);
        assert_eq!(
            progress,
            [
                Progress::Chunk {
                    index: 0,
                    count: 2,
                    source: (0, first.len())
                },
                Progress::Chunk {
                    index: 1,
                    count: 2,
                    source: (first.len() + separator.len(), text.len() - 2),
                },
            ]
        );
    }

    #[test]
    fn leaves_structured_lines_alone() {
        for input in [
            "• Take enough clean paper for every person in the drawing class\n\
             • bring enough pencils for every person in the drawing class\n\
             • find someone to organize the supplies before noon.",
            "1. Take enough clean paper for every person in the drawing class\n\
             2. bring enough pencils for every person in the drawing class\n\
             3. find someone to organize the supplies before noon.",
            "Attendance\nThe volunteers sorted the donated books by subject and left the\n\
             illustrated guides near the door so everyone could find them\n\
             before the workshop started.",
            "Training Office\n158 Garden Lane\nWestmere, Oregon 97721\nUnited States",
            "Morning on the hill\nclouds drift over water\nshadows cross the fields\nthen the world is still.",
            "The workshop supplied clean paper and pencils for every person.\n\
             The organizers arranged the supplies before the class began.\n\
             Everyone took a sheet and a pencil.",
            "Name\tFirst workshop session\nMorning\tPaper and pencils\nAfternoon\tTidy the room.",
            "Volunteer | Morning workshop session\nVisitor | Paper and pencils\nTeacher | Tidy the room.",
            "for entry in entries {\n    let name = entry.name();\n    write_line(name);\n}",
            "Regards,\nAlex Morgan\nTraining Office\nWestmere",
            "The volunteers sorted the donated books by subject and left the\n  \
             illustrated guides near the door so everyone could find them\n\
             before the workshop started.",
        ] {
            assert!(matches!(Reflowed::new(input).text, Cow::Borrowed(_)));
        }
    }

    #[test]
    fn joins_prose_but_leaves_a_list_in_the_same_document() {
        let list = "- Clean the room\n- Bring the supplies\n- Lock the door";
        let text = format!("{WRAPPED_EN}\n\n{list}\n");
        assert_eq!(
            Reflowed::new(&text).text,
            format!("{UNWRAPPED_EN}\n\n{list}\n")
        );
    }

    #[test]
    fn reflows_extracted_pdf_text_with_and_without_blank_lines() {
        // Synthetic PDF page text with varying paragraph spacing and trailing newlines.
        let pages = [
            (
                "en",
                [
                    "Lantern Workshop",
                    "We counted the lantern frames before the workshop\n\
            opened. The wooden rings were painted blue, and the\n\
            paper panels were set out beside them. One small box\n\
            held the well-known pattern from the spring event. It was\n\
            ready.",
                    "The first group worked beside the window. They cut\n\
            several bright panels and gave the spare sheets to the\n\
            people at the far table. Everyone agreed that the little\n\
            lanterns looked cheerful. We went home.",
                    "1. Bring a ruler.\n\
            2. Return the scissors.",
                ],
            ),
            (
                "ja",
                [
                    "ランタン教室",
                    "教室が始まる前に工作用の木枠を数えました。青く\n\
            塗った木の輪のそばに紙を並べました。春の催しで\n\
            使った見本は小さな箱に入れておきました。紙の色を\n\
            選べるように予備の紙もそろえました。準備ができま\n\
            した。",
                    "最初の班は窓の近くの机で作業を始めました。明るい\n\
            色の紙を切り、余った紙は奥の机の人に渡しました。\n\
            できあがったランタンは棚に並べてみんなで眺めまし\n\
            た。箱に道具を片付けました。家に帰りました。",
                    "１．定規を持参。\n\
            ２．はさみを返却。",
                ],
            ),
        ];
        for (language, sections) in pages {
            for (extractor, separator, end) in [
                ("PDFKit", "\n", ""),
                ("Poppler", "\n", "\n\n"),
                ("Poppler tight layout", "\n", "\n"),
                ("Poppler blank layout", "\n\n", "\n"),
            ] {
                let text = sections.join(separator) + end;
                let reflowed = Reflowed::new(&text);
                let ranges = reflowed.paragraphs(&reflowed.text);
                // Heading, two prose paragraphs, and two numbered list items.
                assert_eq!(ranges.len(), 5, "{language}, {extractor}");
                assert!(
                    ranges
                        .iter()
                        .all(|&(start, end, _)| !reflowed.text[start..end].contains('\n')),
                    "{language}, {extractor}"
                );
                assert_eq!(
                    ranges
                        .iter()
                        .filter(|&&(start, end, _)| {
                            reflowed.is_joined_line(&reflowed.text[start..end])
                        })
                        .count(),
                    2,
                    "{language}, {extractor}"
                );
            }
        }
    }

    #[test]
    fn leaves_non_prose_with_regular_line_lengths_alone() {
        for input in [
            "> The supplies for the drawing workshop were in the blue bins\n\
             > beside the folding tables and the cabinet by the stairs\n\
             > underneath the note about where to return the clean towels\n\
             > after all the visitors went home.",
            "2026-01-18 INFO Allocating buffers for the drawing session\n\
             2026-01-18 INFO Placing the remaining boards near the stairs\n\
             2026-01-18 WARN Moving unused paper into the storage room\n\
             2026-01-18 INFO Complete.",
            "00:04 A small note described where to leave the drawing tools\n\
             00:06 before the workshop finished and the doors were closed\n\
             00:08 so the organizers could count the supplies again later\n\
             00:10 that evening.",
            "Lee: The new workshop needs enough supplies for every person\n\
             Pat: lets leave the extra supplies in the cabinet tomorrow\n\
             Lee: please let the organizer know we counted all the boxes\n\
             Pat: thanks.",
            "    write(buffer, lantern_count);\n    write(buffer, lantern_size);\n    write(buffer, room_count);",
            "The distant hills are fading\nAcross a sea of grain\nThe evening bells are ringing\nAnd here we stand again.",
            "一つ目の班は道具を机の上に置いてから作業を始めました。\n\
             二つ目の班は入口で参加者に紙と定規を手渡しました。\n\
             三つ目の班は机の上に残った道具を棚に収めました。\n\
             四つ目の班は会場の片付けが終わってから帰りました。",
        ] {
            assert!(matches!(Reflowed::new(input).text, Cow::Borrowed(_)));
        }
    }

    #[test]
    fn accepts_number_quote_and_bracket_at_wraps_in_running_prose() {
        let input = "The volunteers counted the lantern frames before noon and left\n\
            20 in the shade beside the rest of the supplies for the room.\n\
            \"Take one from the top,\" said the person who opened the door.\n\
            (There was no need for anyone to check their name in advance.)\n\
            Everyone came in carrying their tickets and looking around\n\
            for the workshop volunteers near the door.";
        assert_eq!(Reflowed::new(input).text, input.replace('\n', " "));
    }

    #[test]
    fn hard_wrapped_sentences_stay_whole() {
        let text = "This is a long\nsentence that wraps. And a\nsecond one.";
        let s = split(text, 40, &chars);
        let texts: Vec<_> = s.chunks.iter().map(|c| c.text).collect();
        assert_eq!(
            texts,
            ["This is a long\nsentence that wraps.", "And a\nsecond one."]
        );
        // A sentence longer than the budget is split at its line breaks.
        let s = split(text, 20, &chars);
        let texts: Vec<_> = s.chunks.iter().map(|c| c.text).collect();
        assert_eq!(
            texts,
            [
                "This is a long",
                "sentence that wraps.",
                "And a\nsecond one."
            ]
        );
        assert_eq!(rejoin(&s), text);
    }

    #[test]
    fn single_newline_paragraphs_split_at_line_ends() {
        // Japanese prose: one paragraph per line, no blank lines, indented with U+3000.
        let text = "メロスは激怒した。王を除かねばならぬ。\n　メロスは村を出た。\n「待て」\n　そう言った。";
        let s = split(text, 25, &chars);
        let texts: Vec<_> = s.chunks.iter().map(|c| c.text).collect();
        assert_eq!(
            texts,
            [
                "メロスは激怒した。王を除かねばならぬ。",
                "メロスは村を出た。\n「待て」\n　そう言った。"
            ]
        );
        assert_eq!(s.chunks[0].separator, "\n　");
        assert_eq!(rejoin(&s), text);
        // A line break inside a sentence is not a paragraph break.
        assert!(!ends_sentence("This is a long"));
        assert!(ends_sentence("He said \"no.\""));
        assert!(ends_sentence("「待て」"));
        assert!(!ends_sentence("(see above)"));
    }

    #[test]
    fn long_sentences_split_at_clauses_and_whitespace() {
        let text = "これは、とても長い、一文です";
        let s = split(text, 8, &chars);
        let texts: Vec<_> = s.chunks.iter().map(|c| c.text).collect();
        assert_eq!(texts, ["これは、", "とても長い、", "一文です"]);
        assert_eq!(rejoin(&s), text);

        let text = "one two three four five";
        let s = split(text, 10, &chars);
        assert!(s.chunks.iter().all(|c| chars(c.text) <= 10));
        assert_eq!(rejoin(&s), text);
    }

    #[test]
    fn text_without_boundaries_is_cut_to_fit() {
        let text = "x".repeat(100);
        let s = split(&text, 30, &chars);
        assert!(s.chunks.len() >= 4);
        assert!(s.chunks.iter().all(|c| chars(c.text) <= 30));
        assert_eq!(rejoin(&s), text);
    }

    #[test]
    fn sentences_of_does_not_pack() {
        let s = sentences_of("A b.\nC d. E.\n\nF.");
        let texts: Vec<_> = s.chunks.iter().map(|c| c.text).collect();
        assert_eq!(texts, ["A b.", "C d.", "E.", "F."]);
        assert_eq!(rejoin(&s), "A b.\nC d. E.\n\nF.");
    }

    #[test]
    fn whitespace_only_text_has_no_chunks() {
        let s = split(" \n\n ", 10, &chars);
        assert!(s.chunks.is_empty());
    }
}
