use std::{ops::Range, sync::LazyLock};

use regex::Regex;

const DIGITS: &str = r"[0-9０-９]+(?:[,，][0-9０-９]{3})*(?:[.．][0-9０-９]+)?";
const EN_SCALES: &str = r"trillions?|billions?|millions?|thousands?";
const JP_SCALES: &str = "兆億万千百十";
const DECIMALS: u128 = 1_000_000;

static NUMERIC_EXPRESSION: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(&format!(
        r"(?i)(?:{DIGITS}\s+(?:{EN_SCALES})(?:[, ]+|,?\s+and\s+))+{DIGITS}(?:\s+(?:{EN_SCALES}))?\s*(?:yen|JPY)\b|(?:[¥￥]|\bJPY\s*)?{DIGITS}(?:[{JP_SCALES}](?:{DIGITS})?)*(?:円[%％]?|[%％]|\s+(?:(?:{EN_SCALES})\s+)?(?:yen|JPY)\b)?"
    )).unwrap()
});

static JAPANESE_AMOUNT: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(&format!(r"(?:{DIGITS}[{JP_SCALES}])*(?:{DIGITS})?円")).unwrap());

static JAPANESE_TERM: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(&format!(r"({DIGITS})([{JP_SCALES}]?)")).unwrap());

static ENGLISH_AMOUNT: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(&format!(
        r"(?i)(?P<prefix>\bJPY\s*|[¥￥]\s*)?(?P<value>(?:{DIGITS}\s+(?:{EN_SCALES})(?:,?\s+and\s+|[, ]+))*{DIGITS}(?:\s+(?:{EN_SCALES}))?)(?P<suffix>\s*(?:yen|JPY)\b)?"
    )).unwrap()
});

static ENGLISH_TERM: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(&format!(r"(?i)({DIGITS})(?:\s+({EN_SCALES}))?")).unwrap());

pub(super) fn expressions(text: &str) -> impl Iterator<Item = Range<usize>> + '_ {
    NUMERIC_EXPRESSION.find_iter(text).map(|m| m.range())
}

fn decimal(text: &str) -> Option<u128> {
    let normalized: String = text
        .chars()
        .filter_map(|c| match c {
            '０'..='９' => char::from_u32(u32::from(c) - u32::from('０') + u32::from('0')),
            '．' => Some('.'),
            ',' | '，' => None,
            c => Some(c),
        })
        .collect();
    let (whole, fraction) = normalized.split_once('.').unwrap_or((&normalized, ""));
    let whole = whole.parse::<u128>().ok()?.checked_mul(DECIMALS)?;
    let fraction = fraction.trim_end_matches('0');
    if fraction.is_empty() {
        return Some(whole);
    }
    let zeros = 6usize.checked_sub(fraction.len())?;
    whole.checked_add(fraction.parse::<u128>().ok()? * 10u128.pow(zeros as u32))
}

fn japanese(text: &str) -> Vec<(u128, bool)> {
    JAPANESE_AMOUNT
        .find_iter(text)
        .filter_map(|m| {
            let expression = m.as_str();
            let mut total = 0u128;
            let mut subtotal = 0u128;
            let mut any_digits = false;
            let mut consumed = 0;
            for term in JAPANESE_TERM.captures_iter(expression) {
                let matched = term.get(0)?;
                if matched.start() != consumed {
                    return None;
                }
                any_digits = true;
                consumed = matched.end();
                let amount = decimal(&term[1])?;
                match &term[2] {
                    "兆" | "億" | "万" => {
                        let factor = match &term[2] {
                            "兆" => 1_000_000_000_000,
                            "億" => 100_000_000,
                            _ => 10_000,
                        };
                        total = total
                            .checked_add(subtotal.checked_add(amount)?.checked_mul(factor)?)?;
                        subtotal = 0;
                    }
                    unit => {
                        let factor = match unit {
                            "千" => 1000,
                            "百" => 100,
                            "十" => 10,
                            _ => 1,
                        };
                        subtotal = subtotal.checked_add(amount.checked_mul(factor)?)?;
                    }
                }
            }
            (any_digits && expression.get(consumed..) == Some("円"))
                .then(|| {
                    total
                        .checked_add(subtotal)
                        .map(|value| (value, expression.contains(['兆', '億', '万'])))
                })
                .flatten()
        })
        .collect()
}

fn english(text: &str) -> Vec<(u128, bool)> {
    ENGLISH_AMOUNT
        .captures_iter(text)
        .filter_map(|amount| {
            if amount.name("prefix").is_none() && amount.name("suffix").is_none() {
                return None;
            }
            let mut scaled = false;
            let mut total = 0u128;
            for term in ENGLISH_TERM.captures_iter(&amount["value"]) {
                let scale = term.get(2);
                scaled |= scale.is_some();
                let factor = match scale.map(|m| m.as_str().to_ascii_lowercase()).as_deref() {
                    Some("trillion" | "trillions") => 1_000_000_000_000,
                    Some("billion" | "billions") => 1_000_000_000,
                    Some("million" | "millions") => 1_000_000,
                    Some("thousand" | "thousands") => 1_000,
                    _ => 1,
                };
                total = total.checked_add(decimal(&term[1])?.checked_mul(factor)?)?;
            }
            Some((total, scaled))
        })
        .collect()
}

/// Explicit scaled yen amounts must survive a Japanese/English translation as exact values.
pub(crate) fn matches(source: &str, target: &str, language: &str) -> bool {
    let (source, target) = match language {
        "en" => (japanese(source), english(target)),
        "ja" => (english(source), japanese(target)),
        _ => return true,
    };
    if !source.iter().any(|&(_, scaled)| scaled) {
        return true;
    }
    let sorted_values = |amounts: Vec<(u128, bool)>| {
        let mut values: Vec<_> = amounts.into_iter().map(|(value, _)| value).collect();
        values.sort_unstable();
        values
    };
    sorted_values(source) == sorted_values(target)
}

static DOLLAR_EN: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(&format!(
        r"(?i)(?:US\$|\$|\bUSD\s+)(?P<leading>{DIGITS}(?:\s+(?:{EN_SCALES}))?)|(?P<trailing>{DIGITS}(?:\s+(?:{EN_SCALES}))?)\s+(?:U\.?S\.?\s+)?(?:dollars?|USD)\b"
    )).unwrap()
});

static DOLLAR_JA: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(&format!(
        r"((?:{DIGITS}[{JP_SCALES}])*(?:{DIGITS})?)(?:米ドル|ドル)"
    ))
    .unwrap()
});

static PERCENT: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(&format!(r"({DIGITS})\s*[%％]")).unwrap());

static YEN_RATE_EN: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(&format!(
        r"(?i)({DIGITS})\s+yen\s+(?:per (?:U\.?S\.? )?|to the )(?:dollar|USD)\b"
    ))
    .unwrap()
});

static YEN_RATE_JA: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(&format!(
        r"[1１]\s*(?:米ドル|ドル)\s*(?:=|＝)?\s*({DIGITS})円"
    ))
    .unwrap()
});

pub(crate) fn pdf_matches(source: &str, target: &str, language: &str) -> bool {
    let rates = |regex: &Regex, text: &str| {
        let mut values = regex
            .captures_iter(text)
            .filter_map(|m| decimal(&m[1]))
            .collect::<Vec<_>>();
        values.sort_unstable();
        values
    };
    let (src_rate, dst_rate) = match language {
        "en" => (&*YEN_RATE_JA, &*YEN_RATE_EN),
        "ja" => (&*YEN_RATE_EN, &*YEN_RATE_JA),
        _ => return true,
    };
    let src_rates = rates(src_rate, source);
    if !src_rates.is_empty() && src_rates != rates(dst_rate, target) {
        return false;
    }
    let source = src_rate.replace_all(source, " RATE ");
    let target = dst_rate.replace_all(target, " RATE ");
    let (source, target) = (source.as_ref(), target.as_ref());
    let dollar_en = |s: &str| {
        DOLLAR_EN
            .captures_iter(s)
            .filter_map(|m| {
                let amount = m.name("leading").or_else(|| m.name("trailing"))?;
                english(&format!("{} yen", amount.as_str()))
                    .first()
                    .map(|a| a.0)
            })
            .collect::<Vec<_>>()
    };
    let dollar_ja = |s: &str| {
        DOLLAR_JA
            .captures_iter(s)
            .filter_map(|m| japanese(&format!("{}円", &m[1])).first().map(|a| a.0))
            .collect::<Vec<_>>()
    };
    let (mut a, mut b) = match language {
        "en" => (dollar_ja(source), dollar_en(target)),
        "ja" => (dollar_en(source), dollar_ja(target)),
        _ => return true,
    };
    if !a.is_empty() {
        a.sort_unstable();
        b.sort_unstable();
        if a != b {
            return false;
        }
    }
    let percent = |text: &str| {
        let mut values = PERCENT
            .captures_iter(text)
            .filter_map(|m| decimal(&m[1]))
            .collect::<Vec<_>>();
        values.sort_unstable();
        values
    };
    let p = percent(source);
    (p.is_empty() || p == percent(target)) && matches(source, target, language)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn pdf_keeps_dollar_amounts_and_percentages() {
        assert!(pdf_matches(
            "$3.5 million, 38.5%",
            "350万ドル、38.5％",
            "ja"
        ));
        assert!(!pdf_matches("$3.5 million, 38.5%", "3.5ドル、38.5％", "ja"));
        assert!(pdf_matches(
            "約6,000万円、1ドル160円",
            "about ¥60 million, 160 yen per dollar",
            "en"
        ));
        assert!(!pdf_matches(
            "35%増で約400万ドル",
            "Up 3.5% to about $4 million",
            "en"
        ));
    }
}
