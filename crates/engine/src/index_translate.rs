//! The pinned Index-Translate 2B GGUF's single-turn, non-thinking text template.

use std::fmt::Write as _;

use llama_cpp_2::model::LlamaModel;
use llama_cpp_2::token::LlamaToken;
use llama_cpp_2::vocab::LlamaVocab;
use pfml::Prompt;

use crate::llama::{ModelProfile, resolve_control_token};
use crate::{EngineError, Result};

// The document translator budgets up to 3× the source plus a margin.
// Reserve enough output for a full 1,024-token chunk whose translation expands.
pub(crate) const MAX_OUTPUT_TOKENS: usize = 4096;

const TURN_START: &str = "<|im_start|>";
const TURN_END: &str = "<|im_end|>";
const THINK_START: &str = "<think>";
const THINK_END: &str = "</think>";
const DIRECTION_START: &str = "请将以下";
const DIRECTION_TO: &str = "文本翻译为";
const TRANSLATE_ONLY: &str = "直接输出翻译结果，不要进行任何解释。";
const KEEP_ANCHORS: &str = r#"保留所有 <a i="N"> 和 </a> 标记及其顺序，不要添加、删除或更改标记。"#;
const SOURCE: &str = "〖源文〗";
const CONSTRAINTS: &str = "〖约束要求〗";
const GLOSSARY: &str = "〖硬性要求〗";
const TERMS: &str = "专名/术语对照：";
const BACKGROUND: &str = "〖参考背景〗";
const BACKGROUND_ONLY: &str = "以下内容仅供参考，无需翻译。";
const SOURCE_ONLY: &str = "只输出源文的译文。";

/// Fixed Index-Translate prompt strings, grouped by kind for the corpus probe.
pub const PROMPT_STRINGS: &[(&str, &[&str])] = &[
    ("control", &[TURN_START, TURN_END, THINK_START, THINK_END]),
    (
        "instruction",
        &[
            DIRECTION_START,
            DIRECTION_TO,
            TRANSLATE_ONLY,
            KEEP_ANCHORS,
            SOURCE_ONLY,
        ],
    ),
    ("section", &[SOURCE, CONSTRAINTS]),
    ("context", &[GLOSSARY, TERMS, BACKGROUND, BACKGROUND_ONLY]),
];

pub(crate) fn contains_prompt_leak(source: &str, candidate: &str) -> bool {
    PROMPT_STRINGS
        .iter()
        .flat_map(|(_, strings)| strings.iter().copied())
        .any(|marker| {
            candidate.match_indices(marker).count() > source.match_indices(marker).count()
        })
}

pub(crate) struct Tokens {
    start: LlamaToken,
    pub end_turn: LlamaToken,
    pub thinking: LlamaToken,
    assistant: Vec<LlamaToken>,
}

impl Tokens {
    pub fn resolve(model: &LlamaModel) -> Result<Self> {
        let vocab = model.vocab();
        let start = resolve_control_token(&vocab, TURN_START)?;
        let end_turn = resolve_control_token(&vocab, TURN_END)?;
        let [thinking] = vocab.tokenize(THINK_START.as_bytes(), false, true)[..] else {
            return Err(EngineError::MissingProfileToken(THINK_START));
        };
        // enable_thinking=false in the shipped template inserts an empty reasoning block.
        let assistant = vocab.tokenize(
            format!("\n{TURN_START}assistant\n{THINK_START}\n\n{THINK_END}\n\n").as_bytes(),
            false,
            true,
        );
        Ok(Self {
            start,
            end_turn,
            thinking,
            assistant,
        })
    }

    pub fn tokenize(
        &self,
        vocab: &LlamaVocab<'_>,
        prompt: &Prompt<'_>,
        preserve_office_anchors: bool,
    ) -> Result<Vec<LlamaToken>> {
        let content = user_turn(prompt, preserve_office_anchors)?;
        let mut tokens = vec![self.start];
        vocab.tokenize_into(
            format!("user\n{}", content.trim()).as_bytes(),
            &mut tokens,
            false,
            false,
        );
        tokens.push(self.end_turn);
        tokens.extend_from_slice(&self.assistant);
        Ok(tokens)
    }
}

fn user_turn(prompt: &Prompt<'_>, preserve_office_anchors: bool) -> Result<String> {
    let (source, target) = ModelProfile::IndexTranslate.explicit_direction(prompt.options)?;
    if prompt.options.form.is_some() {
        return Err(EngineError::UnsupportedStyle);
    }
    let mut content = format!(
        "{DIRECTION_START}{}{DIRECTION_TO}{}，{TRANSLATE_ONLY}\n\n",
        source.chinese, target.chinese
    );
    if preserve_office_anchors {
        let _ = write!(content, "{KEEP_ANCHORS}\n\n");
    }
    if prompt.options.context.is_empty()
        && prompt.history.is_empty()
        && prompt.options.glossary.is_empty()
    {
        content.push_str(prompt.text);
        return Ok(content);
    }
    let _ = write!(content, "{SOURCE}\n{}\n\n{CONSTRAINTS}\n", prompt.text);
    if !prompt.options.glossary.is_empty() {
        let _ = write!(content, "{GLOSSARY}{TERMS}");
        for entry in &prompt.options.glossary {
            let _ = write!(
                content,
                "{}→{}、",
                serde_json::json!(&entry.source),
                serde_json::json!(&entry.target)
            );
        }
        content.push('\n');
    }
    if !prompt.options.context.is_empty() || !prompt.history.is_empty() {
        let _ = writeln!(content, "{BACKGROUND}{BACKGROUND_ONLY}");
        if !prompt.options.context.is_empty() {
            let _ = writeln!(content, "{}", prompt.options.context);
        }
        for pair in prompt.history {
            let _ = writeln!(content, "{} → {}", pair.source, pair.translation);
        }
    }
    let _ = write!(content, "\n{SOURCE_ONLY}");
    Ok(content)
}

#[cfg(test)]
mod tests {
    use pfml::{GlossaryEntry, LangCode, TranslationOptions};

    use super::*;

    #[test]
    fn uses_the_requested_direction_and_preserves_glossary_unicode() {
        let mut options = TranslationOptions::new(LangCode::new("ar").unwrap());
        options.source = Some(LangCode::new("en").unwrap());
        let prompt = user_turn(&Prompt::new(&options, "Office"), false).unwrap();
        assert_eq!(
            prompt,
            "请将以下英语文本翻译为阿拉伯语，直接输出翻译结果，不要进行任何解释。\n\nOffice"
        );

        options.glossary.push(GlossaryEntry {
            source: "Office \"A\"\nAnnex".into(),
            target: "مَكْتَب".into(),
        });
        let prompt = user_turn(&Prompt::new(&options, "Office"), false).unwrap();
        assert!(prompt.contains("〖源文〗\nOffice\n\n〖约束要求〗"));
        assert!(prompt.contains(r#""Office \"A\"\nAnnex"→"مَكْتَب""#));
    }

    #[test]
    fn office_anchor_instruction_is_added_only_when_requested() {
        let mut options = TranslationOptions::new(LangCode::new("fr").unwrap());
        options.source = Some(LangCode::new("en").unwrap());
        let text = r#"A <a i="1">sample</a> text."#;
        let prompt = Prompt::new(&options, text);
        let plain = user_turn(&prompt, false).unwrap();
        let office = user_turn(&prompt, true).unwrap();

        assert!(!plain.contains("保留所有"));
        assert!(office.contains(r#"保留所有 <a i="N"> 和 </a> 标记"#));
        assert!(office.ends_with(text));
        assert!(plain.ends_with(text));
    }
}
