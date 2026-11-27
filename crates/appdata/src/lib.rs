//! Shared desktop application code for settings, file locations, logging, device
//! selection, and the few texts both produce.

pub mod applog;
pub mod devices;
pub mod glossary;
pub mod settings;

/// Whether `text` is written in Japanese: it has kana, and kana and kanji make up a tenth of
/// its letters or more. Kanji alone is ambiguous with Chinese, so it takes kana; a text in
/// another language that quotes a few Japanese words (an English manual with `こんにちは` in an
/// example) is not Japanese. A Japanese sentence full of Latin names still is: `npm install
/// --save-dev typescript を実行` is at the limit.
/// "Japanese ⇄ English" translates such text into English and everything else into Japanese.
pub fn looks_japanese(text: &str) -> bool {
    let (mut kana, mut kanji, mut other) = (0usize, 0usize, 0usize);
    for c in text.chars() {
        match c {
            '\u{3040}'..='\u{309f}'
            | '\u{30a0}'..='\u{30ff}'
            | '\u{31f0}'..='\u{31ff}'
            | '\u{ff66}'..='\u{ff9d}' => kana += 1,
            '\u{3400}'..='\u{4dbf}' | '\u{4e00}'..='\u{9fff}' => kanji += 1,
            c if c.is_alphabetic() => other += 1,
            _ => {}
        }
    }
    kana > 0 && (kana + kanji) * 9 >= other
}

/// Text marking where a translation was cut after it kept repeating, in the UI language
/// (see [`settings::UiLanguage::locale`]).
pub fn cut_marker(locale: &str) -> &'static str {
    match locale {
        "ja" => "〔繰り返しが止まらなかったため、この文の訳を打ち切りました〕",
        "zh-Hans" => "〔译文持续重复，已在此截断〕",
        "zh-Hant" => "〔譯文持續重複，已在此截斷〕",
        "ko" => " [번역이 계속 반복되어 여기서 중단했습니다]",
        "es" => " [traducción interrumpida aquí por repetición]",
        "fr" => " [traduction interrompue ici en raison de répétitions]",
        "de" => " [Übersetzung hier wegen Wiederholungen abgebrochen]",
        "pt-BR" => " [tradução interrompida aqui devido a repetições]",
        "id" => " [terjemahan dihentikan di sini karena terus berulang]",
        _ => " [translation cut off here: it kept repeating]",
    }
}

#[cfg(test)]
mod tests {
    use super::looks_japanese;

    #[test]
    fn japanese_takes_kana_and_a_share_of_the_letters() {
        assert!(looks_japanese("会議は来週です"));
        assert!(looks_japanese("カタカナ"));
        assert!(looks_japanese("ｶﾀｶﾅ"));
        assert!(!looks_japanese("Hello, world"));
        // Kanji only: could be Chinese.
        assert!(!looks_japanese("你好世界"));
        // Japanese with Latin names in it.
        assert!(looks_japanese("Rust と GPUI で実装する"));
        assert!(looks_japanese("npm install --save-dev typescript を実行"));
        // English that quotes Japanese.
        let manual = "Two command-line examples run the engine without the GUI. The first \
            translates a text into English, the second lists the models: translate --to en \
            \"こんにちは\"";
        assert!(!looks_japanese(manual));
        assert!(!looks_japanese(""));
    }
}
