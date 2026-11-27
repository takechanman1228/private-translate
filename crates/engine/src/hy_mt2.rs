//! Prompt framing and sampling for Tencent's Hy-MT2 Q4_K_M GGUF.

use std::borrow::Cow;
use std::fmt::Write as _;

use llama_cpp_2::model::LlamaModel;
use llama_cpp_2::sampling::LlamaSampler;
use llama_cpp_2::token::LlamaToken;
use llama_cpp_2::vocab::LlamaVocab;
use pfml::{Form, Prompt};

use crate::Result;
use crate::llama::{ModelProfile, resolve_control_token};

pub(crate) const MAX_OUTPUT_TOKENS: usize = 4096;

// Define and register each fixed part of the prompt together.
macro_rules! prompt_markers {
    ($($name:ident = $value:literal;)*) => {
        $(const $name: &str = $value;)*
        const PROMPT_MARKERS: &[&str] = &[$($name),*];
    };
}

prompt_markers! {
    BEGIN = "<｜hy_begin▁of▁sentence｜>";
    USER = "<｜hy_User｜>";
    ASSISTANT = "<｜hy_Assistant｜>";
    END_TURN = "<｜hy_place▁holder▁no▁2｜>";
    START_7B = "<|startoftext|>";
    ASSISTANT_7B = "<|extra_0|>";
    END_7B = "<|eos|>";
    SOURCE = "[Source Text]";
    BACKGROUND = "[Background Information]";
    HISTORY = "[Earlier translations for reference]";
    GLOSSARY = "Use these fixed translations for matching terms:";
    TRANSLATION_ONLY = "Output only the translation.";
    KEEP_MEANING = "Preserve all meaning and information.";
    STYLE_PLAIN = "The translation style must strictly conform to [常体（だ・である調）].";
    STYLE_POLITE = "The translation style must strictly conform to [敬体（です・ます調）].";
    ANCHOR_INSTRUCTION = r#"Keep each <a i="N"> opening marker and </a> closing marker unchanged."#;
    CHINESE_SOURCE = "〖待翻译文本〗";
    CHINESE_BACKGROUND = "〖背景信息〗";
    CHINESE_HISTORY = "〖参考译文〗";
    CHINESE_GLOSSARY = "请使用以下术语的指定译法：";
    CHINESE_TRANSLATION_ONLY = "注意只需要输出翻译后的结果，不要额外解释：";
    CHINESE_CONTEXT_TRANSLATION_ONLY = "只输出翻译后的正文，不要添加标题或解释。";
    CHINESE_ANCHOR_INSTRUCTION = r#"原样保留每个 <a i="N"> 开始标记和 </a> 结束标记。"#;
}

fn paired_anchors(text: &str) -> impl Iterator<Item = (&str, &str)> {
    text.match_indices(r#"<a i=""#)
        .filter_map(|(start, prefix)| {
            let (id, body) = text[start + prefix.len()..].split_once(r#"">"#)?;
            if id.is_empty() || !id.bytes().all(|byte| byte.is_ascii_digit()) {
                return None;
            }
            let (body, _) = body.split_once("</a>")?;
            if body.contains(r#"<a i=""#) {
                return None;
            }
            Some((
                &text[start..start + prefix.len() + id.len() + 2 + body.len() + 4],
                body,
            ))
        })
}

pub(crate) fn has_anchors(text: &str) -> bool {
    paired_anchors(text).next().is_some()
}

fn visible_text(text: &str) -> Cow<'_, str> {
    let mut anchors = paired_anchors(text).peekable();
    if anchors.peek().is_none() {
        return Cow::Borrowed(text);
    }
    let mut visible = String::with_capacity(text.len());
    let mut cursor = 0;
    for (anchor, body) in anchors {
        let start = anchor.as_ptr() as usize - text.as_ptr() as usize;
        visible.push_str(&text[cursor..start]);
        visible.push_str(body);
        cursor = start + anchor.len();
    }
    visible.push_str(&text[cursor..]);
    Cow::Owned(visible)
}

/// Prompt framing may be quoted in a legitimate source. Reject only surplus occurrences.
pub(crate) fn contains_prompt_leak(
    prompt: &Prompt<'_>,
    preserve_office_anchors: bool,
    frame: &[&str],
    candidate: &str,
) -> bool {
    let candidate = visible_text(candidate);
    if !PROMPT_MARKERS
        .iter()
        .any(|marker| candidate.contains(marker))
    {
        return false;
    }

    let source = visible_text(prompt.text);
    let is_surplus =
        |marker: &str| candidate.matches(marker).count() > source.matches(marker).count();
    let mut leaked = frame.iter().any(|marker| is_surplus(marker));
    if !leaked {
        // Inspect only fixed strings inserted by this turn, excluding user-supplied sections.
        let _ = build_user_turn(prompt, preserve_office_anchors, &mut |marker| {
            leaked |= is_surplus(marker);
        });
    }
    leaked
}

/// Streams text while keeping potential prompt markers out of the output events.
pub(crate) struct PromptLeakStream {
    markers: Vec<(&'static str, usize)>,
    forwarded: usize,
    found_late_leak: bool,
}

impl PromptLeakStream {
    pub fn new(prompt: &Prompt<'_>, preserve_office_anchors: bool) -> Self {
        let source = visible_text(prompt.text);
        let mut markers = Vec::new();
        let mut add = |marker: &'static str| {
            if !markers.iter().any(|&(known, _)| known == marker) {
                markers.push((marker, source.matches(marker).count()));
            }
        };
        FRAME_1_8B.iter().copied().for_each(&mut add);
        let _ = build_user_turn(prompt, preserve_office_anchors, &mut add);
        Self {
            markers,
            forwarded: 0,
            found_late_leak: false,
        }
    }

    /// `None` means the candidate began with a leaked marker. For a later leak, keep the
    /// already-streamed text visible until the full candidate is checked and rewound.
    pub fn push<'a>(&mut self, candidate: &'a str) -> Option<&'a str> {
        if self.found_late_leak {
            return Some("");
        }
        if self.forwarded == 0 {
            let prefix = candidate.trim_start();
            if self
                .markers
                .iter()
                .any(|&(marker, allowed)| allowed == 0 && prefix.starts_with(marker))
            {
                return None;
            }
            if prefix.is_empty()
                || self
                    .markers
                    .iter()
                    .any(|&(marker, allowed)| allowed == 0 && marker.starts_with(prefix))
            {
                return Some("");
            }
        }

        let pending = self.remaining(candidate);
        let mut end = candidate.len();
        'scan: for (offset, _) in pending.char_indices() {
            let tail = &pending[offset..];
            for &(marker, allowed) in &self.markers {
                if let Some(rest) = tail.strip_prefix(marker) {
                    let through_marker = &candidate[..candidate.len() - rest.len()];
                    if allowed == 0 || through_marker.matches(marker).count() > allowed {
                        self.found_late_leak = true;
                        end = self.forwarded + offset;
                        break 'scan;
                    }
                } else if marker.starts_with(tail) {
                    end = self.forwarded + offset;
                    break 'scan;
                }
            }
        }
        let ready = &candidate[self.forwarded..end];
        self.forwarded = end;
        Some(ready)
    }

    pub fn remaining<'a>(&self, candidate: &'a str) -> &'a str {
        &candidate[self.forwarded..]
    }
}

const FRAME_1_8B: &[&str] = &[BEGIN, USER, ASSISTANT];

// The pinned GGUF chat templates frame a single user turn as BEGIN USER text ASSISTANT
// (1.8B) or START_7B text ASSISTANT_7B (7B). Resolve all four 1.8B markers first.
// With no 1.8B markers, require all three 7B markers. Reject partial frames.
// Resolve the frame separately so user text containing markers stays ordinary text.
pub(crate) struct Tokens {
    begin: LlamaToken,
    user: Option<LlamaToken>,
    assistant: LlamaToken,
    pub end_turn: LlamaToken,
}

impl Tokens {
    pub fn resolve(model: &LlamaModel) -> Result<Self> {
        let vocab = model.vocab();
        Self::resolve_with(|marker| resolve_control_token(&vocab, marker))
    }

    fn resolve_with(resolve: impl Fn(&'static str) -> Result<LlamaToken>) -> Result<Self> {
        let legacy = (
            resolve(BEGIN),
            resolve(USER),
            resolve(ASSISTANT),
            resolve(END_TURN),
        );
        if let (Ok(begin), Ok(user), Ok(assistant), Ok(end_turn)) = &legacy {
            return Ok(Self {
                begin: *begin,
                user: Some(*user),
                assistant: *assistant,
                end_turn: *end_turn,
            });
        }
        if legacy.0.is_ok() || legacy.1.is_ok() || legacy.2.is_ok() || legacy.3.is_ok() {
            legacy.0?;
            legacy.1?;
            legacy.2?;
            legacy.3?;
        }
        Ok(Self {
            begin: resolve(START_7B)?,
            user: None,
            assistant: resolve(ASSISTANT_7B)?,
            end_turn: resolve(END_7B)?,
        })
    }

    pub fn should_stop(
        &self,
        token: LlamaToken,
        is_eog: bool,
        is_control: impl FnOnce() -> bool,
    ) -> bool {
        // The 7B GGUF sets eos_token_id to 3, the ordinary dollar token.
        // So the 7B frame stops at the template's <|eos|> or at a control token marked as EOG;
        // the 1.8B frame stops at its end of turn or at any EOG token, as before.
        token == self.end_turn || (is_eog && (self.user.is_some() || is_control()))
    }

    pub fn prompt_frame(&self) -> &[&str] {
        if self.user.is_some() {
            FRAME_1_8B
        } else {
            &[START_7B, ASSISTANT_7B]
        }
    }

    pub fn is_1_8b(&self) -> bool {
        self.user.is_some()
    }

    pub fn tokenize(
        &self,
        vocab: &LlamaVocab<'_>,
        prompt: &Prompt<'_>,
        preserve_office_anchors: bool,
    ) -> Result<Vec<LlamaToken>> {
        let content = user_turn(prompt, preserve_office_anchors)?;
        let mut tokens = vec![self.begin];
        tokens.extend(self.user);
        vocab.tokenize_into(content.as_bytes(), &mut tokens, false, false);
        tokens.push(self.assistant);
        Ok(tokens)
    }
}

fn user_turn(prompt: &Prompt<'_>, preserve_office_anchors: bool) -> Result<String> {
    build_user_turn(prompt, preserve_office_anchors, &mut |_| {})
}

fn build_user_turn(
    prompt: &Prompt<'_>,
    preserve_office_anchors: bool,
    on_marker: &mut dyn FnMut(&'static str),
) -> Result<String> {
    let (source, target) = ModelProfile::HyMt2.explicit_direction(prompt.options)?;
    if prompt.options.form.is_some() && prompt.options.target.as_str() != "ja" {
        return Err(pfml::PromptError::FormRequiresJapanese.into());
    }

    let preserve_anchors = preserve_office_anchors && has_anchors(prompt.text);
    let mut mark = |marker| {
        on_marker(marker);
        marker
    };
    if prompt.options.target.as_str() == "zh-hans"
        || prompt
            .options
            .source
            .as_ref()
            .is_some_and(|code| code.as_str() == "zh-hans")
    {
        return Ok(chinese_user_turn(
            prompt,
            target.chinese,
            preserve_anchors,
            &mut mark,
        ));
    }

    let mut content = String::new();
    if !prompt.options.context.is_empty() {
        content.push_str(mark(BACKGROUND));
        content.push('\n');
        content.push_str(&prompt.options.context);
        content.push_str("\n\n");
    }
    if !prompt.history.is_empty() {
        content.push_str(mark(HISTORY));
        for pair in prompt.history {
            let _ = write!(content, "\n{} → {}", pair.source, pair.translation);
        }
        content.push_str("\n\n");
    }
    let source_header = mark(SOURCE);
    let _ = write!(
        content,
        "Translate only the {source_header} from {} into {}.",
        source.english, target.english
    );
    if prompt.options.target.as_str() == "ko" {
        // The smaller model omitted part of a short English sentence without this instruction.
        content.push(' ');
        content.push_str(mark(KEEP_MEANING));
    }
    content.push(' ');
    content.push_str(mark(TRANSLATION_ONLY));
    if preserve_anchors {
        content.push(' ');
        content.push_str(mark(ANCHOR_INSTRUCTION));
    }
    if let Some(form) = prompt.options.form {
        content.push('\n');
        content.push_str(mark(match form {
            Form::Plain => STYLE_PLAIN,
            Form::Polite => STYLE_POLITE,
        }));
    }
    if !prompt.options.glossary.is_empty() {
        content.push('\n');
        content.push_str(mark(GLOSSARY));
        for entry in &prompt.options.glossary {
            let _ = write!(
                content,
                "\n{} → {}",
                serde_json::json!(&entry.source),
                serde_json::json!(&entry.target)
            );
        }
    }
    let _ = write!(content, "\n\n{source_header}\n{}", prompt.text);
    Ok(content)
}

fn chinese_user_turn(
    prompt: &Prompt<'_>,
    target: &str,
    preserve_anchors: bool,
    mark: &mut dyn FnMut(&'static str) -> &'static str,
) -> String {
    let mut content = String::new();
    if !prompt.options.context.is_empty() {
        let _ = write!(
            content,
            "{}\n{}\n\n",
            mark(CHINESE_BACKGROUND),
            prompt.options.context
        );
    }
    if !prompt.history.is_empty() {
        content.push_str(mark(CHINESE_HISTORY));
        for pair in prompt.history {
            let _ = write!(content, "\n{} → {}", pair.source, pair.translation);
        }
        content.push_str("\n\n");
    }
    if !prompt.options.glossary.is_empty() {
        content.push_str(mark(CHINESE_GLOSSARY));
        for entry in &prompt.options.glossary {
            let _ = write!(
                content,
                "\n{} → {}",
                serde_json::json!(&entry.source),
                serde_json::json!(&entry.target)
            );
        }
        content.push_str("\n\n");
    }
    if preserve_anchors {
        content.push_str(mark(CHINESE_ANCHOR_INSTRUCTION));
        content.push('\n');
    }
    if !prompt.options.context.is_empty() {
        let _ = write!(
            content,
            "请结合背景信息将以下文本翻译为{target}，{}\n{}\n{}",
            mark(CHINESE_CONTEXT_TRANSLATION_ONLY),
            mark(CHINESE_SOURCE),
            prompt.text
        );
    } else {
        let _ = write!(
            content,
            "将以下文本翻译为{target}，{}\n\n{}",
            mark(CHINESE_TRANSLATION_ONLY),
            prompt.text
        );
    }
    content
}

pub(crate) fn sampler(model: &LlamaModel, retry: bool) -> LlamaSampler {
    // Keep Tencent's decoding settings and a stable seed so the same request is repeatable.
    let repeat_penalty = if retry { 1.1 } else { 1.05 };
    LlamaSampler::chain_simple([
        LlamaSampler::penalties(model.n_vocab(), 64, repeat_penalty, 0.0, 0.0),
        LlamaSampler::top_k(20),
        LlamaSampler::top_p(0.6, 1),
        LlamaSampler::temp(0.7),
        LlamaSampler::dist(42),
    ])
}

#[cfg(test)]
mod tests {
    use pfml::{GlossaryEntry, HistoryPair, LangCode, TranslationOptions};

    use super::*;
    use crate::EngineError;

    #[test]
    fn streaming_guard_preserves_a_quoted_marker_and_an_unfinished_prefix() {
        let mut options = TranslationOptions::new(LangCode::new("ja").unwrap());
        options.source = Some(LangCode::new("en").unwrap());
        let prompt = Prompt::new(&options, SOURCE);
        let mut guard = PromptLeakStream::new(&prompt, false);

        assert_eq!(guard.push("[Source"), Some(""));
        assert_eq!(guard.push(SOURCE), Some(SOURCE));
        let candidate = "[Source Text] [Source";
        assert_eq!(guard.push(candidate), Some(" "));
        assert_eq!(guard.remaining(candidate), "[Source");
        assert!(!contains_prompt_leak(&prompt, false, FRAME_1_8B, candidate));

        let candidate = "[Source Text] [Source Text]";
        assert_eq!(guard.push(candidate), Some(""));
        assert!(contains_prompt_leak(&prompt, false, FRAME_1_8B, candidate));
    }

    #[test]
    fn office_instruction_requires_opt_in_and_current_numeric_paired_anchors() {
        for (target, instruction) in [
            ("ja", ANCHOR_INSTRUCTION),
            ("zh-hans", CHINESE_ANCHOR_INSTRUCTION),
        ] {
            let mut options = TranslationOptions::new(LangCode::new(target).unwrap());
            options.source = Some(LangCode::new("en").unwrap());
            let tagged = Prompt::new(&options, r#"<a i="12">Text</a>"#);
            assert!(user_turn(&tagged, true).unwrap().contains(instruction));
            assert!(!user_turn(&tagged, false).unwrap().contains(instruction));

            for text in ["Text", r#"<a i="N">literal</a>"#, r#"<a i="1">no end"#] {
                assert!(
                    !user_turn(&Prompt::new(&options, text), true)
                        .unwrap()
                        .contains(instruction)
                );
            }
        }
    }

    #[test]
    fn markdown_prompts_match_origin_main_byte_for_byte() {
        // Markdown encodes links, bold and inline code with the same anchor syntax as Office.
        let text = r#"See <a i="1">the link</a> and <a i="2">bold</a> with <a i="3">code</a>."#;
        for (target, prefix) in [
            (
                "ja",
                "Translate only the [Source Text] from English into Japanese. Output only the translation.\n\n[Source Text]\n",
            ),
            (
                "zh-hans",
                "将以下文本翻译为简体中文，注意只需要输出翻译后的结果，不要额外解释：\n\n",
            ),
        ] {
            let mut options = TranslationOptions::new(LangCode::new(target).unwrap());
            options.source = Some(LangCode::new("en").unwrap());
            assert_eq!(
                user_turn(&Prompt::new(&options, text), false).unwrap(),
                format!("{prefix}{text}")
            );
        }
    }

    #[test]
    fn rejects_only_markers_inserted_by_the_current_prompt() {
        let mut options = TranslationOptions::new(LangCode::new("en").unwrap());
        options.source = Some(LangCode::new("ja").unwrap());
        let frame = &[START_7B, ASSISTANT_7B];
        let check = |options: &TranslationOptions, candidate| {
            contains_prompt_leak(
                &Prompt::new(options, "【背景情報】"),
                false,
                frame,
                candidate,
            )
        };
        for marker in [SOURCE, TRANSLATION_ONLY, START_7B, ASSISTANT_7B] {
            assert!(check(&options, marker), "{marker}");
        }
        for marker in [
            BACKGROUND,
            HISTORY,
            GLOSSARY,
            KEEP_MEANING,
            STYLE_PLAIN,
            STYLE_POLITE,
            ANCHOR_INSTRUCTION,
            CHINESE_SOURCE,
            CHINESE_BACKGROUND,
            BEGIN,
            END_7B,
        ] {
            assert!(!check(&options, marker), "{marker}");
        }
        options.context =
            "Background with a quoted [Earlier translations for reference] heading".into();
        assert!(check(&options, BACKGROUND));
        assert!(!check(&options, HISTORY));

        options.source = Some(LangCode::new("zh-hans").unwrap());
        for marker in [
            CHINESE_BACKGROUND,
            CHINESE_SOURCE,
            CHINESE_CONTEXT_TRANSLATION_ONLY,
        ] {
            assert!(check(&options, marker), "{marker}");
        }
        assert!(!check(&options, SOURCE));
        options.context.clear();
        assert!(check(&options, CHINESE_TRANSLATION_ONLY));
        assert!(!check(&options, CHINESE_SOURCE));
        assert!(!check(&options, CHINESE_BACKGROUND));
    }

    #[test]
    fn source_occurrences_exempt_the_same_number_of_candidate_markers() {
        let mut options = TranslationOptions::new(LangCode::new("de").unwrap());
        options.source = Some(LangCode::new("en").unwrap());
        let source = r#"A section called <a i="1">[Source</a> Text]."#;
        let prompt = Prompt::new(&options, source);
        assert!(!contains_prompt_leak(
            &prompt,
            true,
            &[],
            "Eine Sektion namens [Source Text]."
        ));
        assert!(contains_prompt_leak(
            &prompt,
            true,
            &[],
            "[Source Text]: [Source Text]"
        ));
        assert!(contains_prompt_leak(&prompt, true, &[], ANCHOR_INSTRUCTION));
        assert!(!contains_prompt_leak(
            &prompt,
            false,
            &[],
            ANCHOR_INSTRUCTION
        ));
    }

    const LEGACY: [&str; 4] = [BEGIN, USER, ASSISTANT, END_TURN];
    const SEVEN_B: [&str; 3] = [START_7B, ASSISTANT_7B, END_7B];

    fn resolve_markers(markers: &[&str]) -> Result<Tokens> {
        Tokens::resolve_with(|marker| {
            markers
                .iter()
                .position(|&candidate| candidate == marker)
                .map(|index| LlamaToken(100 + index as i32))
                .ok_or(EngineError::MissingProfileToken(marker))
        })
    }

    #[test]
    fn selects_complete_frames_and_prefers_the_legacy_frame() {
        let both = [LEGACY.as_slice(), SEVEN_B.as_slice()].concat();
        for markers in [LEGACY.as_slice(), both.as_slice()] {
            let frame = resolve_markers(markers).unwrap();
            assert_eq!(frame.begin, LlamaToken(100));
            assert_eq!(frame.user, Some(LlamaToken(101)));
            assert_eq!(frame.assistant, LlamaToken(102));
            assert_eq!(frame.end_turn, LlamaToken(103));
        }
        let frame = resolve_markers(&SEVEN_B).unwrap();
        assert_eq!(frame.begin, LlamaToken(100));
        assert_eq!(frame.user, None);
        assert_eq!(frame.assistant, LlamaToken(101));
        assert_eq!(frame.end_turn, LlamaToken(102));
    }

    #[test]
    fn rejects_missing_and_partial_frames() {
        for count in 0..LEGACY.len() {
            assert!(resolve_markers(&LEGACY[..count]).is_err());
        }
        for count in 1..LEGACY.len() {
            let mixed = [&LEGACY[..count], SEVEN_B.as_slice()].concat();
            assert!(resolve_markers(&mixed).is_err());
        }
        for count in 1..SEVEN_B.len() {
            assert!(resolve_markers(&SEVEN_B[..count]).is_err());
        }
    }

    #[test]
    fn stops_at_the_frame_end_and_filters_ordinary_eog_only_for_seven_b() {
        let dollar = LlamaToken(3);
        let control = LlamaToken(200);
        let seven_b = resolve_markers(&SEVEN_B).unwrap();
        assert!(!seven_b.should_stop(dollar, true, || false));
        assert!(seven_b.should_stop(control, true, || true));
        assert!(!seven_b.should_stop(control, false, || true));
        assert!(seven_b.should_stop(seven_b.end_turn, false, || true));

        let legacy = resolve_markers(&LEGACY).unwrap();
        assert!(legacy.should_stop(dollar, true, || false));
        assert!(!legacy.should_stop(dollar, false, || false));
        assert!(legacy.should_stop(legacy.end_turn, false, || true));
    }

    #[test]
    fn requires_an_explicit_verified_direction() {
        let mut options = TranslationOptions::new(LangCode::new("en").unwrap());
        for source in [None, Some("en"), Some("zh-hant")] {
            options.source = source.map(|code| LangCode::new(code).unwrap());
            assert!(matches!(
                user_turn(&Prompt::new(&options, "sample"), false),
                Err(EngineError::UnsupportedDirection(ModelProfile::HyMt2))
            ));
        }
    }

    #[test]
    fn chinese_turns_keep_background_glossary_and_source_separate() {
        for (from, to, name) in [("zh-hans", "en", "英语"), ("en", "zh-hans", "简体中文")] {
            let mut options = TranslationOptions::new(LangCode::new(to).unwrap());
            options.source = Some(LangCode::new(from).unwrap());
            let content = user_turn(&Prompt::new(&options, "source"), false).unwrap();
            assert_eq!(
                content,
                format!(
                    "将以下文本翻译为{name}，注意只需要输出翻译后的结果，不要额外解释：\n\nsource"
                )
            );

            options.context = "background".into();
            options.glossary.push(GlossaryEntry {
                source: "term".into(),
                target: "gloss".into(),
            });
            let content = user_turn(&Prompt::new(&options, "source"), false).unwrap();
            assert!(content.starts_with("〖背景信息〗\nbackground\n\n"));
            assert!(content.contains(r#""term" → "gloss""#));
            assert!(content.contains(&format!("请结合背景信息将以下文本翻译为{name}")));
            assert!(content.contains("不要添加标题或解释"));
            assert!(content.ends_with("〖待翻译文本〗\nsource"));
        }
    }

    #[test]
    fn spanish_and_korean_turns_name_the_explicit_direction() {
        for (from, to, direction) in [
            ("en", "es", "from English into Spanish"),
            ("es", "en", "from Spanish into English"),
            ("en", "ko", "from English into Korean"),
            ("ko", "en", "from Korean into English"),
        ] {
            let mut options = TranslationOptions::new(LangCode::new(to).unwrap());
            options.source = Some(LangCode::new(from).unwrap());
            options.context = "background".into();
            options.glossary.push(GlossaryEntry {
                source: "term".into(),
                target: "gloss".into(),
            });
            let content = user_turn(&Prompt::new(&options, "source"), false).unwrap();
            assert!(content.starts_with("[Background Information]\nbackground\n\n"));
            assert!(content.contains(&format!("Translate only the [Source Text] {direction}.")));
            assert!(content.contains("Output only the translation."));
            assert_eq!(
                content.contains("Preserve all meaning and information."),
                to == "ko"
            );
            assert!(content.contains(r#""term" → "gloss""#));
            assert!(content.ends_with("[Source Text]\nsource"));
        }
    }

    #[test]
    fn chinese_turns_place_file_history_before_the_current_source() {
        let mut options = TranslationOptions::new(LangCode::new("zh-hans").unwrap());
        options.source = Some(LangCode::new("en").unwrap());
        let history = [
            HistoryPair {
                source: "first earlier source".into(),
                translation: "first earlier translation".into(),
            },
            HistoryPair {
                source: "second earlier source".into(),
                translation: "second earlier translation".into(),
            },
        ];
        let content = user_turn(
            &Prompt::new(&options, "current source").with_history(&history),
            false,
        )
        .unwrap();
        assert_eq!(
            content,
            concat!(
                "〖参考译文〗\n",
                "first earlier source → first earlier translation\n",
                "second earlier source → second earlier translation\n\n",
                "将以下文本翻译为简体中文，注意只需要输出翻译后的结果，不要额外解释：\n\n",
                "current source"
            )
        );
    }

    #[test]
    fn keeps_instruction_fields_separate_from_text() {
        let mut options = TranslationOptions::new(LangCode::new("ja").unwrap());
        options.source = Some(LangCode::new("en").unwrap());
        options.form = Some(Form::Polite);
        options.glossary.push(GlossaryEntry {
            source: "Project Atlas".into(),
            target: "か\u{3099}く".into(),
        });
        options.context = "Internal project update".into();
        let text = "Project Atlas is ready.";
        let content = user_turn(&Prompt::new(&options, text), false).unwrap();
        assert!(content.contains("English into Japanese"));
        assert!(content.contains("です・ます"));
        assert!(content.contains("\"Project Atlas\" → \"か\u{3099}く\""));
        assert!(content.starts_with("[Background Information]\nInternal project update"));
        assert!(content.ends_with("[Source Text]\nProject Atlas is ready."));
    }
}
