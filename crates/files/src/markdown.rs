//! Translate prose ranges without reserializing Markdown source.
use std::{ops::Range, path::Path, sync::LazyLock};

use pulldown_cmark::{Event, LinkType, Options, Parser, Tag};

use crate::{Document, FileError};

static URL: LazyLock<regex::Regex> = LazyLock::new(|| {
    regex::Regex::new(r#"(?i)(?:[a-z][a-z0-9+.-]*://|www\.|mailto:)[^\s<>`\[\]"']+"#)
        .expect("valid URL expression")
});

fn options() -> Options {
    Options::ENABLE_TABLES
        | Options::ENABLE_TASKLISTS
        | Options::ENABLE_STRIKETHROUGH
        | Options::ENABLE_FOOTNOTES
        | Options::ENABLE_YAML_STYLE_METADATA_BLOCKS
        | Options::ENABLE_PLUSES_DELIMITED_METADATA_BLOCKS
        | Options::ENABLE_MATH
        | Options::ENABLE_HEADING_ATTRIBUTES
}

pub struct Markdown {
    document: Document,
    segments: Vec<Segment>,
    retained: Vec<usize>,
}

// Paragraphs already contain their wrapped continuation lines. Unrelated list
// items must not inherit the meaning of the preceding item (e.g. UI language
// versus OS light/dark theme).
pub const HISTORY_TOKENS: usize = 0;

pub struct MarkdownExport {
    pub bytes: Vec<u8>,
    pub retained_lines: Vec<usize>,
    pub translated: usize,
    pub total: usize,
}

pub struct Segment {
    range: Range<usize>,
    pub text: String,
    pub line: usize,
    anchors: Vec<Anchor>,
}

enum Anchor {
    Atom(String),
    Pair(String, String),
}

struct Fragment {
    range: Range<usize>,
    text: Option<String>,
    pair: Option<(String, String)>,
}

static TOKEN: LazyLock<regex::Regex> = LazyLock::new(|| {
    regex::Regex::new(r#"<a i="([1-9][0-9]*)">|<a i="([1-9][0-9]*)"/>|(</a>)"#)
        .expect("valid anchor expression")
});

impl Segment {
    pub fn has_anchors(&self) -> bool {
        !self.anchors.is_empty()
    }

    pub fn valid(&self, text: &str) -> bool {
        self.restore(text).is_some()
    }

    fn restore(&self, text: &str) -> Option<String> {
        if text.trim().is_empty() {
            return None;
        }
        let mut out = String::new();
        let mut left = 0;
        let mut used = vec![false; self.anchors.len()];
        let mut open = None;
        for m in TOKEN.captures_iter(text) {
            let token = m.get(0)?;
            let gap = &text[left..token.start()];
            if gap.contains("<a i=") || gap.contains("</a>") {
                return None;
            }
            escape(gap, &mut out);
            if let Some(n) = m.get(1).or_else(|| m.get(2)) {
                let index = n.as_str().parse::<usize>().ok()?.checked_sub(1)?;
                if *used.get(index)? || open.is_some() {
                    return None;
                }
                used[index] = true;
                match (&self.anchors[index], m.get(1)) {
                    (Anchor::Pair(prefix, suffix), Some(_)) => {
                        out.push_str(prefix);
                        open = Some((suffix, token.end()));
                    }
                    (Anchor::Atom(raw), None) => out.push_str(raw),
                    _ => return None,
                }
            } else if let Some((suffix, start)) = open.take() {
                if text[start..token.start()].trim().is_empty() {
                    return None;
                }
                out.push_str(suffix);
                // CommonMark cannot close emphasis after punctuation when the
                // following Japanese word touches the delimiter (e.g. **変更…**を).
                if suffix.chars().all(|c| c == '*' || c == '_')
                    && text[start..token.start()]
                        .chars()
                        .next_back()
                        .is_some_and(|c| !c.is_alphanumeric())
                    && text[token.end()..]
                        .chars()
                        .next()
                        .is_some_and(char::is_alphanumeric)
                {
                    out.push(' ');
                }
            } else {
                return None;
            }
            left = token.end();
        }
        let remainder = &text[left..];
        if open.is_some()
            || !used.into_iter().all(|x| x)
            || remainder.contains("<a i=")
            || remainder.contains("</a>")
        {
            return None;
        }
        escape(remainder, &mut out);
        Some(out)
    }
}

impl Markdown {
    pub fn read(path: &Path) -> Result<Self, FileError> {
        Ok(Self::parse(crate::read_text(path)?))
    }

    pub fn parse(document: Document) -> Self {
        let source = document.source();
        let mut fragments: Vec<Fragment> = Vec::new();
        let mut protected = 0;
        let mut stack = Vec::new();
        let mut cells = Vec::new();
        let mut inlines = Vec::new();
        let mut soft_breaks = Vec::new();
        for (event, range) in Parser::new_ext(source, options()).into_offset_iter() {
            let single_line = || !source[range.clone()].contains(['\r', '\n']);
            match event {
                Event::Start(tag) => {
                    let block = matches!(
                        tag,
                        Tag::CodeBlock(_) | Tag::HtmlBlock | Tag::MetadataBlock(_)
                    );
                    let immutable = matches!(
                        tag,
                        Tag::Link {
                            link_type: LinkType::Autolink
                                | LinkType::Email
                                | LinkType::Collapsed
                                | LinkType::Shortcut,
                            ..
                        } | Tag::Image {
                            link_type: LinkType::Collapsed | LinkType::Shortcut,
                            ..
                        }
                    );
                    let pair = matches!(
                        tag,
                        Tag::Emphasis
                            | Tag::Strong
                            | Tag::Strikethrough
                            | Tag::Link { .. }
                            | Tag::Image { .. }
                    ) && !immutable
                        && protected == 0
                        && single_line();
                    if matches!(tag, Tag::TableCell) {
                        cells.push(range.clone());
                    }
                    if immutable && protected == 0 && single_line() {
                        fragments.push(Fragment {
                            range: range.clone(),
                            text: None,
                            pair: None,
                        });
                    }
                    stack.push((block || immutable, pair.then_some((range, fragments.len()))));
                    protected += usize::from(block || immutable);
                }
                Event::End(_) => {
                    let Some((block, pair)) = stack.pop() else {
                        continue;
                    };
                    protected -= usize::from(block);
                    if let Some((span, from)) = pair
                        && let [item] = &mut fragments[from..]
                        && item.text.is_some()
                        && item.pair.is_none()
                        && span.start < item.range.start
                        && item.range.end < span.end
                    {
                        item.pair = Some((
                            source[span.start..item.range.start].into(),
                            source[item.range.end..span.end].into(),
                        ));
                        item.range = span;
                    }
                }
                Event::SoftBreak if protected == 0 => soft_breaks.push(range),
                Event::InlineHtml(_) => {
                    if protected == 0 {
                        inlines.push(range.start);
                    }
                }
                Event::Text(text) if protected == 0 && single_line() => {
                    // Handle contiguous events (escapes and entities can produce several).
                    let raw = &source[range.clone()];
                    if raw == text.as_ref() && !URL.is_match(raw) {
                        if let Some(previous) = fragments.last_mut().filter(|f| {
                            f.range.end == range.start && f.pair.is_none() && f.text.is_some()
                        }) {
                            previous.range.end = range.end;
                            previous.text.as_mut().unwrap().push_str(&text);
                        } else {
                            fragments.push(Fragment {
                                range,
                                text: Some(text.into_string()),
                                pair: None,
                            });
                        }
                    } else {
                        let mut offset = 0;
                        let is_literal = raw == text.as_ref();
                        let urls = URL
                            .find_iter(raw)
                            .map(|m| (m.start(), m.end()))
                            .chain(std::iter::once((raw.len(), raw.len())));
                        if !is_literal {
                            fragments.push(Fragment {
                                range,
                                text: None,
                                pair: None,
                            });
                        } else {
                            for (start, end) in urls {
                                if start > offset {
                                    fragments.push(Fragment {
                                        range: range.start + offset..range.start + start,
                                        text: Some(raw[offset..start].into()),
                                        pair: None,
                                    });
                                }
                                if end > start {
                                    fragments.push(Fragment {
                                        range: range.start + start..range.start + end,
                                        text: None,
                                        pair: None,
                                    });
                                }
                                offset = end;
                            }
                        }
                    }
                }
                Event::Code(_) | Event::InlineMath(_) | Event::FootnoteReference(_)
                    if protected == 0 && single_line() =>
                {
                    fragments.push(Fragment {
                        range,
                        text: None,
                        pair: None,
                    });
                }
                _ => {}
            }
        }
        fragments.sort_by_key(|f| f.range.start);
        cells.sort_by_key(|r| r.start);
        inlines.sort_unstable();
        let mut index = 0;
        let mut cell_index = 0;
        let mut html_index = 0;
        let mut previous = 0;
        let mut line = 1;
        let mut segments = Vec::new();
        let mut retained = Vec::new();
        while index < fragments.len() {
            let start = fragments[index].range.start;
            let find_end = |from| {
                source[from..]
                    .find(['\r', '\n'])
                    .map_or(source.len(), |n| from + n)
            };
            let mut line_end = find_end(start);
            // Only parser-declared soft breaks join physical lines. This keeps
            // paragraph, list, table and explicit line-break boundaries intact.
            let mut next = soft_breaks.partition_point(|r| r.end <= line_end);
            while let Some(soft) = soft_breaks.get(next).filter(|r| r.contains(&line_end)) {
                line_end = find_end(soft.end);
                next += 1;
            }
            let line_start = source[..start].rfind(['\r', '\n']).map_or(0, |n| n + 1);
            line += crate::line_breaks(&source[previous..start]);
            previous = start;
            while cell_index < cells.len() && cells[cell_index].end <= start {
                cell_index += 1;
            }
            let limit = cells
                .get(cell_index)
                .filter(|r| r.contains(&start))
                .map_or(line_end, |r| r.end.min(line_end));
            let from = index;
            index += 1;
            while index < fragments.len() && fragments[index].range.end <= limit {
                index += 1;
            }
            while html_index < inlines.len() && inlines[html_index] < line_start {
                html_index += 1;
            }
            if inlines.get(html_index).is_some_and(|&p| p < line_end) {
                retained.push(line);
                continue;
            }
            let group = &fragments[from..index];
            if !group.iter().any(|f| {
                f.text
                    .as_deref()
                    .is_some_and(|s| s.chars().any(char::is_alphabetic))
            }) {
                continue;
            }
            if source[..start].ends_with('\\') {
                retained.push(line);
                continue;
            }
            let Some(seg) = assemble(group, source, line) else {
                // Unsupported nested inline constructs keep their original spelling.
                retained.push(line);
                continue;
            };
            segments.push(seg);
        }
        Self {
            document,
            segments,
            retained,
        }
    }

    pub fn segments(&self) -> &[Segment] {
        &self.segments
    }

    pub fn converted_encoding(&self) -> Option<&'static str> {
        self.document.converted_encoding
    }

    /// Report skipped paragraphs by their starting line. A wholly unchanged
    /// document is a failed translation and cannot be saved from this path.
    pub fn write(&self, targets: &[Option<String>]) -> Result<MarkdownExport, FileError> {
        if targets.len() != self.segments.len() {
            return Err(FileError::MismatchedResults);
        }
        let source = self.document.source();
        let mut warnings = self.retained.clone();
        let mut changes = Vec::new();
        for (segment, target) in self.segments.iter().zip(targets) {
            let replacement = target
                .as_deref()
                .map(str::trim)
                .filter(|target| *target != segment.text)
                .and_then(|target| segment.restore(target))
                .filter(|replacement| replacement != &source[segment.range.clone()]);
            if let Some(replacement) = replacement {
                changes.push(Change {
                    segment,
                    replacement,
                });
            } else {
                warnings.push(segment.line);
            }
        }
        let mut accepted = vec![true; changes.len()];
        let mut output = render(source, &changes, &accepted);
        if !same_structure(source, &output) {
            accepted.fill(false);
            let mut budget = (64 * 1024 * 1024).max(source.len().saturating_mul(8));
            retain_valid(
                source,
                &changes,
                &mut accepted,
                0..changes.len(),
                &mut budget,
            );
            output = render(source, &changes, &accepted);
        }
        let translated = accepted.iter().filter(|keep| **keep).count();
        if translated == 0 {
            return Err(FileError::NoTranslation);
        }
        warnings.extend(
            changes
                .iter()
                .zip(accepted)
                .filter_map(|(change, keep)| (!keep).then_some(change.segment.line)),
        );
        warnings.sort_unstable();
        warnings.dedup();
        Ok(MarkdownExport {
            bytes: self.document.write_exact(&output),
            retained_lines: warnings,
            translated,
            total: self.segments.len() + self.retained.len(),
        })
    }
}

struct Change<'a> {
    segment: &'a Segment,
    replacement: String,
}

fn render(source: &str, changes: &[Change<'_>], accepted: &[bool]) -> String {
    let mut output = String::with_capacity(source.len());
    let mut end = 0;
    for (change, keep) in changes.iter().zip(accepted) {
        if *keep {
            output.push_str(&source[end..change.segment.range.start]);
            output.push_str(&change.replacement);
            end = change.segment.range.end;
        }
    }
    output.push_str(&source[end..]);
    output
}

/// Try related changes in batches and narrow rejected batches to a single
/// paragraph when needed. Compare the whole parse so neighboring list and
/// reference-definition boundaries are checked as well.
fn retain_valid(
    source: &str,
    changes: &[Change<'_>],
    accepted: &mut [bool],
    range: Range<usize>,
    budget: &mut usize,
) {
    if range.is_empty() || *budget < source.len() {
        return;
    }
    *budget -= source.len();
    accepted[range.clone()].fill(true);
    if same_structure(source, &render(source, changes, accepted)) {
        return;
    }
    accepted[range.clone()].fill(false);
    if range.len() > 1 {
        let middle = range.start + range.len() / 2;
        retain_valid(source, changes, accepted, range.start..middle, budget);
        retain_valid(source, changes, accepted, middle..range.end, budget);
    }
}

fn assemble(group: &[Fragment], source: &str, line: usize) -> Option<Segment> {
    use std::fmt::Write as _;
    let first = group.first()?;
    let last = group.last()?;
    let start = if first.pair.is_none() && first.text.is_some() {
        first.range.start + source[first.range.clone()].len()
            - source[first.range.clone()].trim_start().len()
    } else {
        first.range.start
    };
    let end = if last.pair.is_none() && last.text.is_some() {
        last.range.start + source[last.range.clone()].trim_end().len()
    } else {
        last.range.end
    };
    if start >= end {
        return None;
    }
    let mut text = String::new();
    let mut anchors = Vec::new();
    let mut left = first.range.start;
    for f in group {
        let gap = source.get(left..f.range.start)?;
        let soft = gap.contains(['\r', '\n']);
        if gap
            .chars()
            .any(|c| !(c.is_whitespace() || (soft && c == '>')))
        {
            // A syntax delimiter that was not paired (e.g. deeply nested emphasis).
            return None;
        }
        if soft {
            text.push(' ');
        } else {
            text.push_str(gap);
        }
        match (&f.text, &f.pair) {
            (Some(s), Some((before, after))) => {
                anchors.push(Anchor::Pair(before.clone(), after.clone()));
                write!(&mut text, "<a i=\"{}\">{}</a>", anchors.len(), s).ok()?;
            }
            (Some(s), None) => text.push_str(s),
            (None, None) => {
                anchors.push(Anchor::Atom(source[f.range.clone()].into()));
                write!(&mut text, "<a i=\"{}\"/>", anchors.len()).ok()?;
            }
            _ => return None,
        }
        left = f.range.end;
    }
    Some(Segment {
        range: start..end,
        text: text.trim().into(),
        line,
        anchors,
    })
}

fn escape(text: &str, out: &mut String) {
    for ch in text.chars() {
        match ch {
            '\r' | '\n' | '\t' => out.push(' '),
            // Entities cannot be reinterpreted as Markdown punctuation after decoding.
            '<' | '>' | '&' | '*' | '_' | '\\' | '`' | '|' | '[' | ']' | '$' | '~' | '#' | '!'
            | '+' | '=' | '^' => {
                use std::fmt::Write as _;
                write!(out, "&#{};", ch as u32).expect("writing to a String");
            }
            ch => out.push(ch),
        }
    }
}

fn same_structure(old: &str, new: &str) -> bool {
    let old = Parser::new_ext(old, options());
    let new = Parser::new_ext(new, options());
    let a = old.reference_definitions();
    let b = new.reference_definitions();
    if !a.iter().all(|(label, def)| {
        b.get(label)
            .is_some_and(|other| def.dest == other.dest && def.title == other.title)
    }) || a.iter().count() != b.iter().count()
    {
        return false;
    }
    // Translation may move a code token or link within the same paragraph.
    // Compare block order and the exact multiset of inline constructs separately;
    // Segment::restore additionally requires each protected anchor exactly once.
    let shape = |parser: Parser<'_>| {
        let mut blocks = Vec::new();
        let mut inline = Vec::new();
        for event in parser {
            let movable = matches!(
                &event,
                Event::Code(_)
                    | Event::InlineMath(_)
                    | Event::Start(
                        Tag::Emphasis
                            | Tag::Strong
                            | Tag::Strikethrough
                            | Tag::Link { .. }
                            | Tag::Image { .. }
                    )
                    | Event::End(
                        pulldown_cmark::TagEnd::Emphasis
                            | pulldown_cmark::TagEnd::Strong
                            | pulldown_cmark::TagEnd::Strikethrough
                            | pulldown_cmark::TagEnd::Link
                            | pulldown_cmark::TagEnd::Image
                    )
            );
            if movable {
                inline.push(format!("{event:?}"));
            } else if !matches!(event, Event::Text(_) | Event::SoftBreak) {
                blocks.push(format!("{event:?}"));
            }
        }
        inline.sort_unstable();
        (blocks, inline)
    };
    shape(old) == shape(new)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn japanese_emphasis_and_reordered_inline_tokens_keep_structure() {
        let source = "- **Change…** in Settings uses `model.gguf`.\n  Keep this filename.\n";
        let md = Markdown::parse(Document::open(source.as_bytes(), "sample.md").unwrap());
        assert_eq!(md.segments().len(), 1);
        let result = md
            .write(&[Some(
                "<a i=\"2\"/>を使うには<a i=\"1\">変更…</a>を押します。ファイル名は保持します。"
                    .into(),
            )])
            .unwrap();
        let output = String::from_utf8(result.bytes).unwrap();
        assert!(output.contains("**変更…** を"));
        assert!(output.contains("`model.gguf`"));
        assert_eq!(result.translated, 1);
        assert!(result.retained_lines.is_empty());
        assert!(!same_structure(
            source,
            "- **Change…** uses `wrong.gguf`.\n"
        ));
        assert!(!same_structure(
            source,
            "1. **Change…** uses `model.gguf`.\n"
        ));
    }

    #[test]
    fn wraps_prose_as_one_unit_and_retains_only_the_unsafe_edit() {
        let source =
            "Apply if sales are low.\nAbove that, use another license.\n\nSecond paragraph.\n";
        let md = Markdown::parse(Document::open(source.as_bytes(), "sample.md").unwrap());
        assert_eq!(md.segments().len(), 2);
        assert_eq!(
            md.segments()[0].text,
            "Apply if sales are low. Above that, use another license."
        );
        let export = md
            .write(&[Some("別のライセンス".into()), Some("1. Unsafe list".into())])
            .unwrap();
        assert_eq!(
            export.bytes,
            "別のライセンス\n\nSecond paragraph.\n".as_bytes()
        );
        assert_eq!(export.retained_lines, [4]);
    }

    #[test]
    fn protected_source_and_real_markdown_survive() {
        let source = "\u{feff}---\r\ntitle: 原文\r\n---\r\n# 見出し\r\n\r\n> **重要** [資料][doc] ![画像](pic.png) `コード` $x+y$\r\n\r\n[doc]: https://example.org/a\r\n\r\n- [x] 先頭\r\n  - ネスト\r\n\r\n| 項目 | 結果 |\r\n| --- | --- |\r\n| A | 完了 |\r\n\r\n```xml\r\n<a>触らない</a>\r\n```\r\n\r\n<details>\r\n原文\r\n</details>\r\n\r\nURL https://example.org/b を開く。\r\n";
        let md = Markdown::parse(Document::open(source.as_bytes(), "sample.md").unwrap());
        let identity: Vec<_> = md.segments().iter().map(|s| Some(s.text.clone())).collect();
        assert!(!identity.is_empty());
        assert!(matches!(md.write(&identity), Err(FileError::NoTranslation)));
        let mut translated = identity.clone();
        for (segment, target) in md.segments().iter().zip(&mut translated) {
            if segment.text == "見出し" || segment.text == "完了" {
                *target = Some("English | value".into());
            }
        }
        let export = md.write(&translated).unwrap();
        assert_eq!(export.translated, 2);
        let output = String::from_utf8(export.bytes).unwrap();
        assert_eq!(
            output,
            source
                .replace("見出し", "English &#124; value")
                .replace("完了", "English &#124; value")
        );
        assert!(
            identity
                .iter()
                .flatten()
                .all(|text| !text.contains("example.org")
                    && !text.contains("触らない")
                    && !text.contains("コード"))
        );
    }

    #[test]
    fn translating_labels_preserves_reference_targets_and_inline_shapes() {
        let source = "[資料][doc] and [doc]\n\n[doc]: https://example.org\n";
        let doc = Markdown::parse(Document::open(source.as_bytes(), "s.md").unwrap());
        assert!(!doc.segments().iter().any(|s| s.text == "doc"));
        let output: Vec<_> = doc
            .segments()
            .iter()
            .map(|s| Some(s.text.replace("資料", "Report")))
            .collect();
        assert_eq!(doc.segments().len(), 1);
        assert!(doc.segments()[0].has_anchors());
        assert!(!doc.segments()[0].valid("Report"));
        let export = doc.write(&output).unwrap();
        assert_eq!(
            export.bytes,
            b"[Report][doc] and [doc]\n\n[doc]: https://example.org\n"
        );
        assert!(export.retained_lines.is_empty());
    }
}
