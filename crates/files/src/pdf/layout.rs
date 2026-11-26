//! Finds paragraphs from the positions of PDF text. It infers paragraph rhythm and applies
//! CJK boundary rules to positioned text independently of PDFium.
//!
//! A piece of text ([`Item`]) is in points from the page's top left, `y` being its baseline.
//! Pieces on one baseline make a line, and lines of one size, close together and aligned, make a
//! paragraph. Formulas are told by their fonts (TeX's math fonts and the like) and symbols: a
//! line that is mostly formula is left as drawn, and a formula inside a paragraph is a part of
//! its own ([`Part::Math`]).
//!
//! Text lengths are measured in UTF-16 code units. Text trimming and whitespace checks use
//! the ECMAScript whitespace set.

use std::sync::LazyLock;

use regex::Regex;

/// A run of positioned text on one baseline.
#[derive(Clone, Debug, PartialEq)]
pub struct Item {
    pub str: String,
    pub x: f64,
    /// The baseline.
    pub y: f64,
    pub w: f64,
    pub size: f64,
    pub top: f64,
    pub bottom: f64,
    /// A name that tells the piece's font apart from the page's others.
    pub font: String,
    pub bold: bool,
    pub italic: bool,
    pub mono: bool,
    pub serif: bool,
    pub math: bool,
    /// An index the caller uses to locate the original piece.
    pub id: usize,
}

#[derive(Clone, Debug)]
pub struct Line {
    pub items: Vec<Item>,
    pub left: f64,
    pub right: f64,
    pub y: f64,
    pub size: f64,
    pub top: f64,
    pub bottom: f64,
    /// Characters, not counting white space.
    pub chars: usize,
    /// Mostly formula.
    pub math: bool,
    pub bold: bool,
    pub italic: bool,
    pub serif: bool,
    pub mono: bool,
}

#[derive(Clone, Debug)]
pub struct Block {
    pub lines: Vec<Line>,
    /// The distances between the baselines of the lines.
    pub gaps: Vec<f64>,
    pub left: f64,
    pub right: f64,
    pub top: f64,
    pub bottom: f64,
    pub size: f64,
}

#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Rect {
    pub left: f64,
    pub right: f64,
    pub top: f64,
    pub bottom: f64,
}

/// What a paragraph holds: text, or a formula inside it.
#[derive(Clone, Debug, PartialEq)]
pub enum Part {
    Text(String),
    Math {
        rect: Rect,
        baseline: f64,
        size: f64,
        /// The index of its line in the block.
        line: usize,
    },
}

impl Part {
    pub fn text(&self) -> Option<&str> {
        match self {
            Part::Text(s) => Some(s),
            Part::Math { .. } => None,
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Align {
    Left,
    Center,
    Justify,
}

impl Align {
    pub fn as_str(self) -> &'static str {
        match self {
            Align::Left => "left",
            Align::Center => "center",
            Align::Justify => "justify",
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Default)]
pub struct FontKind {
    pub mono: bool,
    pub serif: bool,
    pub bold: bool,
    pub italic: bool,
}

/// What a PDF says of a font; each is `None` where it says nothing.
#[derive(Clone, Copy, Debug, Default)]
pub struct FontFlags {
    pub mono: Option<bool>,
    pub serif: Option<bool>,
    pub bold: Option<bool>,
    pub italic: Option<bool>,
}

fn re(pattern: &str) -> Regex {
    Regex::new(pattern).expect("a valid pattern")
}

/// A font name's pattern, matched as JavaScript's `/…/i` does (no `u` flag): ASCII letters only
/// are taken as either case. Rust's `(?i)` folds by Unicode (`ſ` matches `s`, `K` (Kelvin)
/// matches `k`), so the pattern and the name are lowered to ASCII instead ([`Ci::is_match`]).
struct Ci(Regex);

impl Ci {
    fn new(pattern: &str) -> Self {
        Self(re(&pattern.to_ascii_lowercase()))
    }

    fn is_match(&self, name: &str) -> bool {
        self.0.is_match(&name.to_ascii_lowercase())
    }
}

// Fonts of formulas: TeX's (Computer Modern, AMS, Latin Modern, STIX…), Word's (Cambria Math), Symbol.
static MATH_FONT: LazyLock<Ci> = LazyLock::new(|| {
    Ci::new(
        r"CMMI|CMSY|CMEX|CMBSY|MSAM|MSBM|EUFM|EUSM|EUEX|RSFS|STMARY|WASY|ESINT|BBM|DSROM|MTSY|MTMI|MTEX|TXSY|TXMI|TXEX|PXSY|PXMI|PXEX|Math|(^|\+)Symbol",
    )
});
// Symbols of formulas: Greek, primes, arrows, operators, mathematical letters.
static MATH_CHAR: LazyLock<Regex> = LazyLock::new(|| {
    re(
        r"[\x{0370}-\x{03FF}\x{2032}-\x{2037}\x{2061}-\x{2064}\x{2190}-\x{21FF}\x{2200}-\x{22FF}\x{2308}-\x{230B}\x{27C0}-\x{27EF}\x{2980}-\x{2AFF}\x{1D400}-\x{1D7FF}]",
    )
});
// Names of functions set upright in formulas.
static MATH_WORD: LazyLock<Regex> = LazyLock::new(|| {
    re(
        r"^(?:log|ln|exp|sin|cos|tan|tanh|max|min|arg|argmax|argmin|lim|sup|inf|det|dim|Pr|softmax|sign|diag|tr|mod)$",
    )
});
static LETTER: LazyLock<Regex> = LazyLock::new(|| re(r"\p{L}"));
static LOWER_START: LazyLock<Regex> = LazyLock::new(|| re(r"^\p{Ll}"));
static WORD3: LazyLock<Regex> = LazyLock::new(|| re(r"\p{L}{3,}"));
static WORD2: LazyLock<Regex> = LazyLock::new(|| re(r"\p{L}{2,}"));
static BOLD: LazyLock<Ci> = LazyLock::new(|| Ci::new(r"Bold|Black|Heavy|Semibold|Demi|CMBX|Medi"));
static ITALIC: LazyLock<Ci> = LazyLock::new(|| Ci::new(r"Italic|Oblique|CMTI|CMSL"));
// Marks that start a list item or a footnote: a paragraph of its own.
// Full-width numbers too, as Japanese documents number their items: `（１）`, `１．`, `⑴`.
static BULLET: LazyLock<Regex> = LazyLock::new(|| {
    re(
        r"^(?:[•⚫●○■□◆◇・▪▶➢※◦‣∙·*∗†‡§¶①-⑳⑴-⒇◻❑❖✓►▸⋆★☆]|[0-9]{1,2}[.)]\s|\([0-9]{1,2}\)\s*|（[0-9０-９]{1,2}）|[０-９]{1,2}[．）])",
    )
});
/// Characters that only mark a list item or a footnote. A piece of these alone (with digits
/// and commas, as in `1,∗`) is not a formula, whatever its font: a symbol font draws bullets,
/// and TeX's symbol font footnote stars.
const MARKS: &str = "•⚫●○■□◆◇▪▫▶►▸➢※◦‣∙·*∗†‡§¶◻❑❖✓⋆★☆";
// A numbered heading (`3.1 Results`, `A.2 Proofs`): a paragraph of its own among bold lines.
static HEADING: LazyLock<Regex> = LazyLock::new(|| {
    re(&format!(
        r"^(?:[0-9]+(?:\.[0-9]+)*\.?|[A-Z]\.[0-9]*(?:\.[0-9]+)*){JS_SPACE}+\p{{Lu}}"
    ))
});
/// JavaScript's `\s` as a class: Rust's has U+0085 and not U+FEFF. (`\d` is written `[0-9]`:
/// JavaScript's is ASCII, Rust's every decimal digit, `１` too.)
const JS_SPACE: &str = r"[\t\n\x0B\x0C\r \x{A0}\x{1680}\x{2000}-\x{200A}\x{2028}\x{2029}\x{202F}\x{205F}\x{3000}\x{FEFF}]";
// `Mono(?!type)` is handled in `mono_font` because Rust regexes do not support look-ahead.
static MONO_FONT: LazyLock<Ci> = LazyLock::new(|| {
    Ci::new(
        r"Courier|Consol|Typewriter|NimbusMon|Menlo|Monaco|Inconsolata|SourceCode|Andale|LucidaConsole|Terminal|OCR[AB]|(?:^|\+)(?:CM|SF|EC|LM)(?:SL|IT|VT)?TT[0-9]",
    )
});
static SANS_FONT: LazyLock<Ci> = LazyLock::new(|| {
    Ci::new(
        r"Sans|Gothic|Helvet|Arial|Verdana|Tahoma|Calibri|Segoe|Roboto|Meiryo|CMSS|Univers|Futura|Frutiger|Myriad|Avenir|Lato|Hiragino ?Kaku|HiraKaku|YuGo|Osaka|ゴシック|メイリオ|黑体|Hei(?:ti)?(?:$|[^a-z])",
    )
});
static SERIF_FONT: LazyLock<Ci> = LazyLock::new(|| {
    Ci::new(
        r"Times|Roman|NimbusRom|Serif|Mincho|Ming|Song|SimSun|Garamond|Georgia|Palatino|Bookman|Century|Schoolbook|Minion|Charter|Utopia|Cambria|Caslon|Bodoni|Didot|Libertine|STIX|Baskerville|Antiqua|Ryumin|明朝|宋体|(?:^|\+)(?:CM(?:R|BX|TI|SL|CSC|U|DUNH|B)|SF(?:RM|BX|TI|SL)|EC(?:RM|BX|TI|SL))[0-9]",
    )
});

/// Characters joined without spaces. Hangul uses word spacing.
pub fn is_cjk(ch: char) -> bool {
    matches!(ch,
        '\u{2e80}'..='\u{2fdf}' | '\u{3000}'..='\u{30ff}' | '\u{31f0}'..='\u{31ff}' |
        '\u{3400}'..='\u{4dbf}' | '\u{4e00}'..='\u{9fff}' | '\u{f900}'..='\u{faff}' |
        '\u{fe30}'..='\u{fe4f}' | '\u{ff01}'..='\u{ff60}' | '\u{ff61}'..='\u{ff9f}' |
        '\u{ffe0}'..='\u{ffee}' | '\u{1aff0}'..='\u{1afff}' | '\u{1b000}'..='\u{1b16f}' |
        '\u{20000}'..='\u{2a6df}' | '\u{2a700}'..='\u{2b73f}' | '\u{2b740}'..='\u{2b81f}' |
        '\u{2b820}'..='\u{2ceaf}' | '\u{2ceb0}'..='\u{2ebef}' | '\u{2ebf0}'..='\u{2ee5f}' |
        '\u{2f800}'..='\u{2fa1f}' | '\u{30000}'..='\u{3134f}' | '\u{31350}'..='\u{323af}' |
        '\u{323b0}'..='\u{3347f}')
}

/// JavaScript's white space (`\s`, `trim`): Unicode's but U+0085 (NEL), and U+FEFF.
fn js_space(ch: char) -> bool {
    (ch.is_whitespace() && ch != '\u{85}') || ch == '\u{FEFF}'
}

fn js_trim(s: &str) -> &str {
    s.trim_matches(js_space)
}

/// JavaScript's `length`: UTF-16 code units.
fn js_len(s: &str) -> usize {
    s.encode_utf16().count()
}

fn without_space(s: &str) -> String {
    s.chars().filter(|&c| !js_space(c)).collect()
}

/// JavaScript's `toFixed(1)`: the value as it is held (binary64, not the decimal it was written
/// as) to the nearer tenth, a tie away from 0. Rust's `{:.1}` rounds that value too, but a tie to
/// even; a tie is only a value held exactly halfway between two tenths, an odd number of quarters
/// (`10.25`, not `8.35`, held a little below 8.35, which `x * 10.0` would round up to `83.5`).
pub(crate) fn fixed1(x: f64) -> String {
    let quarters = x.abs() * 4.0;
    if quarters < 2f64.powi(53) && quarters.fract() == 0.0 && quarters % 2.0 == 1.0 {
        // Exact: an odd number of quarters times 10 is a whole number and a half.
        let tenths = (x.abs() * 10.0).ceil().copysign(x);
        return format!("{:.1}", tenths / 10.0);
    }
    format!("{x:.1}")
}

fn mono_font(name: &str) -> bool {
    // `/Mono(?!type)/i`: ASCII letters only in either case, as JavaScript without `u`.
    let lower = name.to_ascii_lowercase();
    let mut from = 0;
    while let Some(at) = lower[from..].find("mono") {
        let after = from + at + 4;
        if !lower[after..].starts_with("type") {
            return true;
        }
        from = after;
    }
    MONO_FONT.is_match(name)
}

/// What a font is, from its name. `flags` are what the PDF says, which count where the name
/// tells nothing.
pub fn font_kind(name: &str, flags: FontFlags) -> FontKind {
    let sans = SANS_FONT.is_match(name);
    let roman = SERIF_FONT.is_match(name);
    let mono = mono_font(name) || (flags.mono.unwrap_or(false) && !sans && !roman);
    let serif = !mono && !sans && (roman || flags.serif.unwrap_or(false));
    FontKind {
        mono,
        serif,
        bold: flags.bold.unwrap_or(false) || BOLD.is_match(name),
        italic: flags.italic.unwrap_or(false) || ITALIC.is_match(name),
    }
}

const KANGXI: &str = "一丨丶丿乙亅二亠人儿入八冂冖冫几凵刀力勹匕匚匸十卜卩厂厶又口囗土士夂夊夕大女子宀寸小尢尸屮山巛工己巾干幺广廴廾弋弓彐彡彳心戈戶手支攴文斗斤方无日曰月木欠止歹殳毋比毛氏气水火爪父爻爿片牙牛犬玄玉瓜瓦甘生用田疋疒癶白皮皿目矛矢石示禸禾穴立竹米糸缶网羊羽老而耒耳聿肉臣自至臼舌舛舟艮色艸虍虫血行衣襾見角言谷豆豕豸貝赤走足身車辛辰辵邑酉釆里金長門阜隶隹雨靑非面革韋韭音頁風飛食首香馬骨高髟鬥鬯鬲鬼魚鳥鹵鹿麥麻黃黍黑黹黽鼎鼓鼠鼻齊齒龍龜龠";

/// The bullet that a symbol font (Symbol, Wingdings) draws for a private-use code point, which
/// a PDF gives for its list marks. Shared by both PDF extractions.
pub fn marker_char(ch: char) -> Option<char> {
    Some(match ch {
        '\u{f0b7}' => '•',
        '\u{f0a7}' => '▪',
        '\u{f0a8}' => '◻',
        '\u{f06e}' => '■',
        '\u{f071}' => '❑',
        '\u{f076}' => '❖',
        '\u{f0d8}' => '➢',
        '\u{f0fc}' => '✓',
        _ => return None,
    })
}

/// Text as the model should read it. PDFs made from CJK fonts often give radicals (⽣, U+2F00…)
/// for the characters that look the same (生), and ligatures (ﬁ) for their letters. This is
/// NFKC for those ranges only. Map a symbol font's bullet to the Unicode bullet.
pub fn normalize(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for ch in s.chars() {
        if let Some(mark) = marker_char(ch) {
            out.push(mark);
            continue;
        }
        match ch {
            '\u{2F00}'..='\u{2FD5}' => {
                out.push(KANGXI.chars().nth(ch as usize - 0x2F00).unwrap_or(ch));
            }
            '\u{2E9F}' => out.push('母'),
            '\u{2EF3}' => out.push('龟'),
            '\u{FB00}' => out.push_str("ff"),
            '\u{FB01}' => out.push_str("fi"),
            '\u{FB02}' => out.push_str("fl"),
            '\u{FB03}' => out.push_str("ffi"),
            '\u{FB04}' => out.push_str("ffl"),
            '\u{FB05}' | '\u{FB06}' => out.push_str("st"),
            _ => out.push(ch),
        }
    }
    out
}

/// Whether a piece is formula, by its font's name or its symbols. List and footnote marks
/// alone are not, even in a symbol font.
pub fn is_math(name: &str, s: &str) -> bool {
    !marks_only(s) && (MATH_FONT.is_match(name) || math_symbols(s))
}

/// A list bullet or footnote mark, possibly with digits and commas (`1,∗`), and nothing else.
pub fn marks_only(s: &str) -> bool {
    let s = js_trim(s);
    s.chars().any(|c| MARKS.contains(c))
        && s.chars()
            .all(|c| MARKS.contains(c) || c.is_ascii_digit() || c == ',' || js_space(c))
}

// Symbols of prose as much as of formulas: arrows, comparisons, a minus, a degree.
fn prose_symbol(s: &str) -> bool {
    let s = js_trim(s);
    !s.is_empty()
        && s.chars().count() <= 2
        && s.chars().all(|c| {
            matches!(
                c,
                '\u{2190}'
                    ..='\u{21FF}'
                        | '∼'
                        | '≈'
                        | '≤'
                        | '≥'
                        | '±'
                        | '×'
                        | '÷'
                        | '−'
                        | '∞'
                        | '°'
                        | '′'
                        | '″'
                        | '…'
            )
        })
}

// A single Greek letter (not a formula font's): a symbol named in prose, `the α parameter`.
fn greek_letter(it: &Item) -> bool {
    let mut chars = js_trim(&it.str).chars();
    chars
        .next()
        .is_some_and(|c| matches!(c, '\u{0391}'..='\u{03A9}' | '\u{03B1}'..='\u{03C9}'))
        && chars.next().is_none()
        && !MATH_FONT.is_match(&it.font)
}

// Units of measure, with or without an SI prefix (`km`, `kPa`, `mol`): words of a calculation.
static UNIT: LazyLock<Regex> = LazyLock::new(|| {
    re(
        r"^(?:[kMGTmµμnpcdh]?(?:m|g|s|l|L|Hz|Pa|J|W|Wh|V|A|Ah|N|Ω|K|eV|mol|cd|lm|lx|bar|B|bps)|ppm|ppb|dB|atm|cal|kcal|psi|rpm|mph|dpi|sec|hr)$",
    )
});
// Element symbols one after another (`NaCl`, `AgCl`, `NaOH`): a chemical formula. Capitals
// alone (`PDF`, and `KOH` too) are told apart in `token_kind`.
static ELEMENTS: LazyLock<Regex> = LazyLock::new(|| re(r"^(?:\p{Lu}\p{Ll}?){2,}$"));

/// How a word of a line reads, to tell a sentence from a formula drawn in a text font.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Token {
    /// A word of prose: two letters or more, not one of the kinds below.
    Word,
    Number,
    /// A word of a formula: letters with digits (`AgNO3`, `x1`), element symbols (`NaCl`), a
    /// unit (`km`), a function's name (`sin`).
    Formula,
    /// Either: one letter (`a`, `x`), capitals (`PDF`, `KOH`), a capital and a letter (`In`,
    /// `Na`).
    Either,
}

fn token_kind(t: &str) -> Token {
    let letters = t.chars().filter(|c| c.is_alphabetic()).count();
    let lower = t.chars().any(char::is_lowercase);
    if letters == 0 {
        Token::Number
    } else if letters < t.chars().count() {
        Token::Formula
    } else if letters == 1 {
        Token::Either
    } else if MATH_WORD.is_match(t) || UNIT.is_match(t) || (lower && ELEMENTS.is_match(t)) {
        Token::Formula
    } else if !lower || (letters == 2 && t.starts_with(char::is_uppercase)) {
        Token::Either
    } else {
        Token::Word
    }
}

/// The words of a text (letters and digits together) and its CJK characters, which stand
/// for words of their own: `None`.
fn tokens(s: &str) -> Vec<Option<&str>> {
    let mut out = Vec::new();
    let mut start = None;
    for (i, c) in s.char_indices() {
        let cjk = is_cjk(c) && c.is_alphabetic();
        if c.is_alphanumeric() && !cjk {
            start.get_or_insert(i);
            continue;
        }
        if let Some(b) = start.take() {
            out.push(Some(&s[b..i]));
        }
        if cjk {
            out.push(None);
        }
    }
    if let Some(b) = start {
        out.push(Some(&s[b..]));
    }
    out
}

/// Whether the word of `s` next to a sign (its last when `last`, else its first) is one of a
/// sentence: a word of prose, a number, or CJK text, and not a word of a formula (`AgNO3`,
/// `NaCl`, `km`, `x`).
fn prose_beside(s: &str, last: bool) -> bool {
    let words = tokens(s);
    let word = if last { words.last() } else { words.first() };
    match word {
        Some(Some(t)) => matches!(token_kind(t), Token::Word | Token::Number),
        Some(None) => true,
        None => false,
    }
}

/// Whether a line reads as a sentence rather than a formula: its words of prose (two CJK
/// characters for one) are two or more, and outnumber its formula pieces, formula words
/// (`NaCl`, `AgNO3`, `km`, `sin`) and operators (`+`, `=`). Numbers count for neither.
fn reads_as_prose(line: &Line) -> bool {
    let mut text = String::new();
    let mut formula = 0usize;
    let mut prev: Option<&Item> = None;
    for it in &line.items {
        if js_trim(&it.str).is_empty() {
            continue;
        }
        if it.math {
            formula += 1;
            text.push(' ');
        } else {
            if prev.is_some_and(|p| it.x - (p.x + p.w) > 0.15 * line.size) {
                text.push(' ');
            }
            text.push_str(&it.str);
        }
        prev = Some(it);
    }
    formula += text.chars().filter(|c| "+=<>^＋＝".contains(*c)).count();
    let mut prose = 0.0;
    for t in tokens(&text) {
        match t.map(token_kind) {
            None => prose += 0.5,
            Some(Token::Word) => prose += 1.0,
            Some(Token::Formula) => formula += 1,
            Some(Token::Number | Token::Either) => {}
        }
    }
    prose >= 2.0 && prose > formula as f64
}

fn math_symbols(s: &str) -> bool {
    let s = js_trim(s);
    !s.is_empty()
        && js_len(s) <= 4
        && MATH_CHAR.is_match(s)
        && !s.chars().any(|ch| {
            let mut buf = [0; 4];
            let c = ch.encode_utf8(&mut buf);
            LETTER.is_match(c) && !MATH_CHAR.is_match(c)
        })
}

/// Pieces on one baseline, next to each other, make a line; a wide gap (between columns) ends
/// it. Formula pieces only need to overlap the line: radicals, fractions, and large operators
/// sit above and below it, and the parts of a fraction stand over one another.
pub fn lines_of(items: Vec<Item>) -> Vec<Line> {
    let mut lines: Vec<Line> = Vec::new();
    let mut current: Option<usize> = None;
    for it in items {
        let same = current.is_some_and(|i| {
            let line = &lines[i];
            let size = line.size.max(it.size);
            let on_baseline = (it.y - line.y).abs() < 0.6 * size;
            let overlaps = it.math && it.top < line.bottom && it.bottom > line.top;
            let after = it.x > line.right - (if it.math { 3.0 } else { 0.6 }) * size
                && it.x - line.right < 1.2 * size;
            (on_baseline || overlaps) && after
        });
        if same {
            let line = &mut lines[current.unwrap()];
            line.right = line.right.max(it.x + it.w);
            line.top = line.top.min(it.top);
            line.bottom = line.bottom.max(it.bottom);
            // The baseline is that of the main text, not of an index or exponent.
            if it.size > line.size * 1.05 {
                line.size = it.size;
                line.y = it.y;
            }
            line.items.push(it);
        } else {
            lines.push(Line {
                left: it.x,
                right: it.x + it.w,
                y: it.y,
                size: it.size,
                top: it.top,
                bottom: it.bottom,
                items: vec![it],
                chars: 0,
                math: false,
                bold: false,
                italic: false,
                serif: false,
                mono: false,
            });
            current = Some(lines.len() - 1);
        }
    }
    for line in &mut lines {
        describe(line);
    }
    lines
}

/// Marks the formula pieces of a line, and what the line mostly is.
fn describe(line: &mut Line) {
    // An arrow or a sign between two words of prose (`Text → .rtf`), or a Greek letter of a
    // text font, is a part of the sentence, not a formula: first, before formulas take in what
    // is beside them. Only in a line that reads as a sentence, and between words of prose or
    // numbers: a formula drawn in a text font has words too (`NaCl + AgNO3 → AgCl + NaNO3`,
    // `5 km/h × 2 h`), and stays a formula.
    let words = |it: Option<&Item>| it.is_some_and(|it| !it.math && WORD2.is_match(&it.str));
    let sentence = reads_as_prose(line);
    for i in 0..line.items.len() {
        let demote = {
            let items = &line.items;
            let it = &items[i];
            let solid = |j: &usize| !js_trim(&items[*j].str).is_empty();
            let prev = (0..i).rev().find(solid).map(|j| &items[j]);
            let next = (i + 1..items.len()).find(solid).map(|j| &items[j]);
            let sized = |o: Option<&Item>| o.is_some_and(|o| it.size >= 0.8 * o.size);
            it.math
                && sentence
                && (prose_symbol(&it.str) || greek_letter(it))
                && words(prev)
                && words(next)
                && sized(prev)
                && sized(next)
                && prev.is_some_and(|p| prose_beside(&p.str, true))
                && next.is_some_and(|n| prose_beside(&n.str, false))
        };
        if demote {
            line.items[i].math = false;
        }
    }
    // Digits, signs, and function names beside a formula piece belong to the formula, as do the
    // small pieces next to it (indices, exponents).
    for _ in 0..2 {
        for i in 0..line.items.len() {
            let it = &line.items[i];
            if it.math {
                continue;
            }
            let s = js_trim(&it.str);
            if s.is_empty() {
                continue;
            }
            let close = 0.6 * line.size;
            let prev = i.checked_sub(1).map(|j| &line.items[j]);
            let next = line.items.get(i + 1);
            let beside = prev.is_some_and(|p| p.math && it.x - (p.x + p.w) < close)
                || next.is_some_and(|n| n.math && n.x - (it.x + it.w) < close);
            if !beside {
                continue;
            }
            let symbolic = !LETTER.is_match(s) || MATH_WORD.is_match(s);
            let small = it.size < 0.8 * line.size;
            if symbolic || small {
                line.items[i].math = true;
            }
        }
    }
    let mut chars = 0;
    let mut math = 0;
    let mut words = 0;
    // Preserve discovery order so the first of equal weights wins.
    let mut weight: Vec<(String, usize)> = Vec::new();
    for it in &line.items {
        let n = js_len(&without_space(&it.str));
        chars += n;
        if it.math {
            math += n;
        } else {
            words += WORD3.find_iter(&it.str).count();
            let k = format!("{}|{}", it.font, fixed1(it.size));
            match weight.iter_mut().find(|(key, _)| *key == k) {
                Some((_, w)) => *w += n,
                None => weight.push((k, n)),
            }
        }
    }
    line.chars = chars;
    // Mostly formula, or a formula with a word or two (`Attention(Q, K, V) = …`).
    line.math = chars > 0
        && (math as f64 / chars as f64 > 0.5
            || (words == 0 && math > 0)
            || (words <= 2 && math >= 3 && math as f64 >= 0.3 * chars as f64));
    // The line's main font: that of most of its text.
    let mut main: Option<&(String, usize)> = None;
    for entry in &weight {
        if main.is_none_or(|m| entry.1 > m.1) {
            main = Some(entry);
        }
    }
    let main = main.map(|(k, _)| k.clone());
    let it = line
        .items
        .iter()
        .find(|i| {
            !i.math && main.as_deref() == Some(format!("{}|{}", i.font, fixed1(i.size)).as_str())
        })
        .unwrap_or(&line.items[0]);
    let (size, bold, italic, serif, mono) = (it.size, it.bold, it.italic, it.serif, it.mono);
    line.size = size;
    line.bold = bold;
    line.italic = italic;
    line.serif = serif;
    line.mono = mono;
}

/// Same-format neighboring lines, including an ordinary first-line or hanging indent.
fn same_run(a: &Line, b: &Line) -> bool {
    !a.math
        && !b.math
        && a.chars >= 12
        && b.chars > 0
        && (b.size - a.size).abs() <= 0.15 * a.size
        && a.bold == b.bold
        && a.mono == b.mono
        && (b.left - a.left).abs() <= 2.0 * a.size
        && (0.5..=3.0).contains(&((b.y - a.y) / a.size))
        && !starts_item(&spaced_text(b))
}

/// Runs must show wrapping, not just equally spaced standalone labels. An indent or a
/// shorter line supplies that evidence; at least two lines must reach the right edge.
fn prose_runs(lines: &[Line]) -> Vec<&[Line]> {
    let mut runs = Vec::new();
    let mut start = 0;
    for end in 1..=lines.len() {
        if end < lines.len() && same_run(&lines[end - 1], &lines[end]) {
            continue;
        }
        let run = &lines[start..end];
        if run.len() >= 3 {
            let size = run[0].size;
            let right = run
                .iter()
                .map(|l| l.right)
                .fold(f64::NEG_INFINITY, f64::max);
            let left = run.iter().map(|l| l.left).fold(f64::INFINITY, f64::min);
            let indented = run[0].left > left + 0.8 * size;
            let short_end = run.iter().any(|l| l.right < right - size);
            let full = run.iter().filter(|l| l.right >= right - size).count();
            if full >= 2 && (indented || short_end) {
                runs.push(run);
            }
        }
        start = end;
    }
    runs
}

/// Upper bound of the inferred body rhythm's ±8% band (default: 1.75 em).
/// Only runs of at least three lines with evidence of wrapping contribute. Nineteen
/// measured intervals are required; they need not come from one continuous run.
pub fn leading_limit(pages: &[Vec<Line>]) -> f64 {
    let mut bins = std::collections::BTreeMap::<i32, usize>::new();
    let mut samples = 0;
    for lines in pages {
        for run in prose_runs(lines) {
            for pair in run.windows(2) {
                if pair[1].chars < 12 {
                    continue;
                }
                let gap = (pair[1].y - pair[0].y) / pair[0].size;
                *bins.entry((gap / 0.05).round() as i32).or_default() += 1;
                samples += 1;
            }
        }
    }
    if samples < 19 {
        return 1.75;
    }
    let mode = bins
        .iter()
        .max_by_key(|(bin, count)| (**count, -**bin))
        .unwrap()
        .0;
    (f64::from(*mode) * 0.05 * 1.08).max(1.75)
}

/// Lines of one size and weight, close below one another and aligned, make a paragraph.
/// `page_width` is the page's, in points.
pub fn blocks_of(lines: Vec<Line>, page_width: f64) -> Vec<Block> {
    let leading = leading_limit(std::slice::from_ref(&lines));
    blocks_with_leading(lines, page_width, leading)
}

pub fn blocks_with_leading(lines: Vec<Line>, page_width: f64, leading: f64) -> Vec<Block> {
    // Right edges are local to a column and style, and supported by actual wrapped runs.
    let edges: Vec<_> = prose_runs(&lines)
        .iter()
        .map(|run| {
            let mut rights: Vec<_> = run.iter().map(|l| l.right).collect();
            rights.sort_by(f64::total_cmp);
            (run[0].clone(), rights[(rights.len() - 1) * 3 / 4])
        })
        .collect();
    let keys: Vec<(f64, f64, Key)> = lines
        .iter()
        .filter(|ln| !left_out(ln))
        .filter_map(|ln| key_of(&spaced_text(ln)).map(|k| (ln.left, ln.size, k)))
        .collect();
    let mut blocks: Vec<Block> = Vec::new();
    let mut open = false;
    for ln in lines {
        // Formulas on lines of their own stay as drawn, and end the paragraph.
        if ln.math || ln.chars == 0 {
            open = false;
            continue;
        }
        if open
            && continues(
                blocks.last().unwrap(),
                &ln,
                page_width,
                leading,
                &edges,
                &keys,
            )
        {
            let block = blocks.last_mut().unwrap();
            let last_y = block.lines.last().unwrap().y;
            block.gaps.push(ln.y - last_y);
            block.left = block.left.min(ln.left);
            block.right = block.right.max(ln.right);
            block.top = block.top.min(ln.top);
            block.bottom = block.bottom.max(ln.bottom);
            block.lines.push(ln);
        } else {
            blocks.push(Block {
                gaps: Vec::new(),
                left: ln.left,
                right: ln.right,
                top: ln.top,
                bottom: ln.bottom,
                size: ln.size,
                lines: vec![ln],
            });
            open = true;
        }
    }
    blocks
}

/// The lines that were left out of every paragraph (formulas on lines of their own, empty
/// lines), for a caller that keeps them as drawn. Same rule as in [`blocks_of`].
pub fn left_out(line: &Line) -> bool {
    line.math || line.chars == 0
}

fn continues(
    block: &Block,
    ln: &Line,
    page_width: f64,
    leading: f64,
    edges: &[(Line, f64)],
    keys: &[(f64, f64, Key)],
) -> bool {
    let last = block.lines.last().unwrap();
    let size = block.size;
    let first = &block.lines[0];
    // Bold words inside a sentence can make most of a line bold: the weights differ only
    // where a line is all of one weight (a heading), or the first line, mixed, ends short.
    // A whole bold line inside a regular sentence goes on with it: the line before is
    // unfinished (and full, for a paragraph's first line) and the bold line begins as the
    // rest of a sentence.
    // A bold first line followed by a line that mixes weights is a bold lead-in that ended
    // inside the paragraph. Subsequent regular lines continue the sentence.
    let emphasis = (mixed_weight(first)
        && (block.lines.len() > 1 || first.right >= ln.right - size))
        || (first.bold && block.lines[1..].iter().any(mixed_weight))
        || (mixed_weight(ln) && !first.bold)
        || (!first.bold
            && ln.bold
            && (block.lines.len() > 1 || first.right >= ln.right - size)
            && ends_open(&spaced_text(last))
            && starts_continuation(&spaced_text(ln)));
    if (ln.size - size).abs() > 0.15 * size
        || (ln.bold != first.bold && !emphasis)
        || ln.mono != first.mono
    {
        return false;
    }
    let gap = ln.y - last.y;
    if gap < 0.5 * size || gap > leading.min(2.6) * size {
        return false;
    }
    // Extra leading is justified only by the body rhythm AND a previous wrapped line.
    // This also applies to the very first join, before the block has its own right edge.
    if gap > 1.75 * size {
        let edge = edges
            .iter()
            .filter(|(l, _)| {
                (l.size - size).abs() <= 0.15 * size
                    && l.bold == first.bold
                    && l.mono == first.mono
                    && (l.left - first.left).abs() <= 2.0 * size
            })
            .map(|(_, right)| *right)
            .max_by(f64::total_cmp);
        if gap < leading / 1.08 * 0.92 * size || edge.is_none_or(|right| last.right < right - size)
        {
            return false;
        }
    }
    if !block.gaps.is_empty() && gap > 1.3 * median(&block.gaps) {
        return false;
    }
    let text = spaced_text(ln);
    let text = text.trim_start_matches(js_space);
    if BULLET.is_match(text) {
        return false;
    }
    // Headings one below the other (`3. Method`, `3.1 Data`) are each their own.
    if first.bold && HEADING.is_match(text) {
        return false;
    }
    let first_text = spaced_text(first);
    // Entries of a schedule or a chronology, each line beginning with a date (`10/27：…`).
    if entries(keys, (&first_text, first.left), (text, ln.left), size) {
        return false;
    }
    let marked = starts_item(&first_text);
    // A footnote or item whose first line, full and unfinished, goes on below its mark from
    // the left edge.
    let wraps_left = block.lines.len() == 1
        && marked
        && ln.left < first.left
        && first.left - ln.left <= 2.5 * size
        && first.right >= ln.right - size
        && ends_open(&first_text);
    // A single list item is not evidence of a hanging continuation when the next
    // line moves left, or when the item ended well short of the next line's edge.
    // Full first lines still join their genuine indented continuation.
    if block.lines.len() == 1
        && marked
        && (ln.left - first.left).abs() > 0.8 * size
        && ((ln.left < first.left && !wraps_left)
            || (first.right < ln.right - 3.0 * size
                && !text.starts_with("https://")
                && !text.starts_with("http://")))
    {
        return false;
    }
    // A centered paragraph (a title, a notice): its lines begin anywhere.
    if centered(block, ln, page_width) {
        return true;
    }
    // Indented: the first line of the next paragraph. Elsewhere: another column or box.
    let hanging = marked && ln.left >= first.left && ln.left <= first.left + 2.5 * size;
    // A full, unfinished first line and the next line indented under it: a hanging indent
    // (a reference, a note after its label `(注) 1.`), not the first line of a paragraph.
    // Not a title over a centered subtitle in another size or style: a narrower line
    // centered under the first, as far in from its left as from its right (a hanging line
    // reaches the right or ends short).
    let centered_below = ln.left - first.left >= 0.5 * size
        && ((ln.left - first.left) - (first.right - ln.right)).abs() <= 0.3 * size
        && ((ln.size - first.size).abs() > 0.03 * size || ln.italic != first.italic);
    let full_open = block.lines.len() == 1
        && !first.bold
        && first.right >= ln.right - size
        && ends_open(&first_text)
        && !centered_below;
    let labelled = full_open
        && (ln.left <= first.left + 2.5 * size
            || label_column(first).is_some_and(|x| (ln.left - x).abs() <= 0.5 * size));
    // The later lines of a hanging paragraph go on at the left of its second line.
    let follows = block.lines.len() >= 2
        && block.lines[1].left > first.left + 0.8 * size
        && (ln.left - block.lines[1].left).abs() <= 0.8 * size;
    if (!hanging && !labelled && !follows && ln.left > block.left + 0.8 * size)
        || ln.left < block.left - 4.0 * size
    {
        return false;
    }
    if ln.right < block.left || ln.left > block.right {
        return false;
    }
    // The line before ended short of the others: the paragraph ended there, unless its
    // sentence goes on and the next line's first word would not have fitted in the room
    // left (ragged-right text wraps a long word early).
    if block.lines.len() >= 2
        && last.right < block.right - 3.0 * size
        && !(ends_open(&spaced_text(last))
            && first_word_width(ln).is_some_and(|w| w > block.right - last.right - 0.3 * size))
    {
        return false;
    }
    true
}

/// The width of a line's first word, in proportion to the characters of its first piece.
fn first_word_width(ln: &Line) -> Option<f64> {
    let it = ln.items.iter().find(|it| !js_trim(&it.str).is_empty())?;
    let s = js_trim(&it.str);
    let word = s.split(js_space).next()?;
    if word.chars().any(is_cjk) {
        return None;
    }
    Some(it.w * js_len(word) as f64 / js_len(s).max(1) as f64)
}

fn middle(left: f64, right: f64) -> f64 {
    (left + right) / 2.0
}

/// Whether `ln` goes on a centered paragraph: around the same middle as all its lines, and
/// beginning well apart from them (an indent is smaller, and moves the middle). Only wide text:
/// short centered lines below one another are a name, a place, an address, each its own.
fn centered(block: &Block, ln: &Line, page_width: f64) -> bool {
    let size = block.size;
    if (block.right - block.left).max(ln.right - ln.left) < 0.45 * page_width {
        return false;
    }
    if (ln.left - block.left).abs() <= 2.5 * size {
        return false;
    }
    let c = middle(ln.left, ln.right);
    block
        .lines
        .iter()
        .all(|l| (middle(l.left, l.right) - c).abs() <= 0.5 * size)
}

fn median(values: &[f64]) -> f64 {
    let mut sorted = values.to_vec();
    sorted.sort_by(f64::total_cmp);
    sorted[sorted.len() / 2]
}

/// A line's text with a space where its pieces are apart, as [`parts_of`] sets it: list marks
/// and numbered headings are told by this text (`2.` and `Engan` are often two pieces).
fn spaced_text(ln: &Line) -> String {
    let mut out = String::new();
    let mut prev: Option<&Item> = None;
    for it in &ln.items {
        if let Some(p) = prev
            && it.x - (p.x + p.w) > 0.15 * ln.size
            && !out.ends_with(js_space)
            && !it.str.starts_with(js_space)
        {
            out.push(' ');
        }
        out.push_str(&it.str);
        prev = Some(it);
    }
    out
}

// A note's label: `(注)`, a number, a reference mark, or these together (`(注) 1.`, `※1`).
static NOTE_LABEL: LazyLock<Regex> = LazyLock::new(|| {
    re(
        r"^(?:[（(]?注[0-9０-９]*[）)]?)?(?:[0-9０-９]{1,2}[.．)）]?|[（(\[][0-9０-９]{1,2}[）)\]]|[※*∗†‡§¶•▪■●◆①-⑳]+[0-9０-９]*)?$",
    )
});

/// Where the text after a note's label begins on a line (`(注) 1.`, `※1`, `[12]`): the
/// left of the first piece after a label of at most six characters and a gap of 0.3 em.
fn label_column(line: &Line) -> Option<f64> {
    let mut label = String::new();
    let mut prev: Option<&Item> = None;
    for it in &line.items {
        if js_trim(&it.str).is_empty() {
            continue;
        }
        if let Some(p) = prev
            && it.x - (p.x + p.w) >= 0.3 * line.size
        {
            return NOTE_LABEL.is_match(&label).then_some(it.x);
        }
        label.push_str(&without_space(&it.str));
        if js_len(&label) > 6 {
            return None;
        }
        prev = Some(it);
    }
    None
}

/// Whether a line mixes bold and regular words (emphasis inside a sentence).
fn mixed_weight(line: &Line) -> bool {
    let words = |bold: bool| {
        line.items
            .iter()
            .any(|it| !it.math && it.bold == bold && WORD2.is_match(&it.str))
    };
    words(true) && words(false)
}

// Shared by both PDF extractions: how one piece of text goes on from another.

/// Whether a list item or a footnote starts the text (a bullet, `1. `, `(2)`, `①`).
pub fn starts_item(text: &str) -> bool {
    BULLET.is_match(text.trim_start_matches(js_space))
}

// The key that begins an entry of a schedule, a chronology or a list: a date (`5/27`,
// `2026-05-27`, `27.05.`, `5月27日`, `令和7年`), a time (`10:30`), an ordinal (`第21回`). Not a
// number alone, nor a decimal (`3.5`): wrapped prose begins with those too.
static KEY: LazyLock<Regex> = LazyLock::new(|| {
    re(
        r"^(?:(?P<date>[0-9０-９]{1,4}[/／][0-9０-９]{1,2}(?:[/／][0-9０-９]{1,4})?|[0-9]{4}-[0-9]{1,2}(?:-[0-9]{1,2})?|[0-9]{1,2}\.[0-9]{1,2}\.(?:[0-9]{2,4})?|(?:令和|平成|昭和)?[0-9０-９元]{1,4}年(?:[0-9０-９]{1,2}月(?:[0-9０-９]{1,2}日)?)?|[0-9０-９]{1,2}月(?:[0-9０-９]{1,2}日)?)|(?P<time>[0-9０-９]{1,2}[:：][0-9０-９]{2})|(?P<ordinal>第[0-9０-９]{1,3}[回条章節項号]))(?:$|[^0-9０-９])",
    )
});

/// What begins an entry of a schedule, a chronology or a list.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Key {
    Date,
    Time,
    Ordinal,
}

/// The key that text begins with, if any: a date, a time or an ordinal.
pub fn key_of(text: &str) -> Option<Key> {
    let found = KEY.captures(text.trim_start_matches(js_space))?;
    Some(if found.name("date").is_some() {
        Key::Date
    } else if found.name("time").is_some() {
        Key::Time
    } else {
        Key::Ordinal
    })
}

/// Whether the lines that begin with `first` at `first_left` and with `next` at `next_left`
/// are entries of one list (a schedule, a chronology: `10/27：…`, `11/27：…`), each its own
/// paragraph: both begin with a key of one kind, about at one left (2 em, as the keys may be
/// set to the right), as do three or more lines of the page there in that size. `keys` holds
/// the left, size and key of each line of the page that begins with one. Shared by both PDF
/// extractions.
pub fn entries(
    keys: &[(f64, f64, Key)],
    (first, first_left): (&str, f64),
    (next, next_left): (&str, f64),
    size: f64,
) -> bool {
    let Some(key) = key_of(next) else {
        return false;
    };
    let listed = keys
        .iter()
        .filter(|(x, s, k)| {
            *k == key && (x - next_left).abs() <= 2.0 * size && (s - size).abs() <= 0.15 * size
        })
        .count();
    key_of(first) == Some(key) && (next_left - first_left).abs() <= 2.0 * size && listed >= 3
}

/// Whether text ends without a sentence's last punctuation (closing brackets aside).
pub fn ends_open(text: &str) -> bool {
    let t = text.trim_end_matches(|c: char| js_space(c) || "」』）)】]〕\"'”’»".contains(c));
    t.chars()
        .next_back()
        .is_some_and(|c| !"。．.!?！？:：;；".contains(c))
}

// Hiragana that usually opens a Japanese sentence rather than going on with one.
const OPENERS: [&str; 26] = [
    "なお",
    "また",
    "さらに",
    "この",
    "その",
    "これ",
    "それ",
    "ただし",
    "しかし",
    "あわせて",
    "まず",
    "こう",
    "そう",
    "ここ",
    "そこ",
    "いず",
    "もし",
    "よって",
    "したがって",
    "ちなみに",
    "とくに",
    "つまり",
    "すなわち",
    "および",
    "もっとも",
    "いわゆる",
];

/// Whether text begins as the rest of a sentence: a lowercase letter, or (in CJK) a comma, a
/// closing bracket, a small kana or the prolonged sound mark, or hiragana that does not
/// usually open a sentence.
pub fn starts_continuation(text: &str) -> bool {
    let t = text.trim_start_matches(js_space);
    let Some(c) = t.chars().next() else {
        return false;
    };
    c.is_lowercase()
        || "、，,）)」』】〕ーぁぃぅぇぉっゃゅょゎァィゥェォッャュョヮ".contains(c)
        || (('ぁ'..='ゖ').contains(&c) && !OPENERS.iter().any(|o| t.starts_with(o)))
}

/// How a line's text meets the text of the line before it.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Joint {
    /// A word hyphenated at the end of the line, joined again (`compli-` `cated`).
    Dehyphenate,
    /// CJK characters, which meet without a space.
    Tight,
    Space,
}

/// The joint of two lines' texts, shared by both PDF extractions (white space at the ends of
/// `before` and `next` aside).
pub fn joint(before: &str, next: &str) -> Joint {
    let before = before.trim_end_matches(js_space);
    let next = next.trim_start_matches(js_space);
    let mut b = before.chars();
    let hyphen = b.next_back() == Some('-')
        && b.next_back()
            .is_some_and(|c| LETTER.is_match(c.encode_utf8(&mut [0; 4])));
    if hyphen && LOWER_START.is_match(next) {
        Joint::Dehyphenate
    } else if before.chars().next_back().is_some_and(is_cjk)
        && next.chars().next().is_some_and(is_cjk)
    {
        Joint::Tight
    } else {
        Joint::Space
    }
}

/// The text of two lines joined by their [`joint`].
pub fn join_lines(before: &str, next: &str) -> String {
    let b = before.trim_end_matches(js_space);
    let n = next.trim_start_matches(js_space);
    match joint(b, n) {
        Joint::Dehyphenate => format!("{}{n}", &b[..b.len() - 1]),
        Joint::Tight => format!("{b}{n}"),
        Joint::Space if b.is_empty() || n.is_empty() => format!("{b}{n}"),
        Joint::Space => format!("{b} {n}"),
    }
}

/// The usual distance between a paragraph's baselines, in its font size (1.15 for one line).
pub fn line_height(block: &Block) -> f64 {
    if block.gaps.is_empty() {
        1.15
    } else {
        median(&block.gaps) / block.size
    }
}

/// The paragraph's content: text, and formulas inside it. Lines are joined with a space (none
/// between CJK characters), and a word hyphenated at the end of a line is joined again.
pub fn parts_of(block: &Block) -> Vec<Part> {
    let mut parts: Vec<Part> = Vec::new();
    fn text(parts: &mut Vec<Part>, s: &str) {
        if let Some(Part::Text(last)) = parts.last_mut() {
            last.push_str(s);
        } else {
            parts.push(Part::Text(s.to_owned()));
        }
    }
    fn tail(parts: &[Part]) -> &str {
        match parts.last() {
            Some(Part::Text(s)) => s,
            _ => "",
        }
    }
    let ends_space = |s: &str| s.chars().next_back().is_some_and(js_space);
    for (i, ln) in block.lines.iter().enumerate() {
        if i > 0 {
            let first = ln.items.iter().find(|it| !js_trim(&it.str).is_empty());
            let next = match first {
                Some(f) if !f.math => f.str.trim_start_matches(js_space).to_owned(),
                _ => String::new(),
            };
            let before = tail(&parts).trim_end_matches(js_space).to_owned();
            match joint(&before, &next) {
                Joint::Dehyphenate => {
                    if let Some(Part::Text(last)) = parts.last_mut() {
                        *last = before[..before.len() - 1].to_owned();
                    }
                }
                Joint::Tight => {}
                Joint::Space => text(&mut parts, " "),
            }
        }
        let mut prev: Option<&Item> = None;
        for it in &ln.items {
            let gap = prev.map_or(0.0, |p| it.x - (p.x + p.w));
            let spaced = gap > 0.15 * ln.size;
            if it.math {
                let joins = prev.is_some_and(|p| p.math);
                match parts.last_mut() {
                    Some(Part::Math { rect, .. }) if joins => {
                        rect.left = rect.left.min(it.x);
                        rect.right = rect.right.max(it.x + it.w);
                        rect.top = rect.top.min(it.top);
                        rect.bottom = rect.bottom.max(it.bottom);
                    }
                    _ => {
                        if spaced && !ends_space(tail(&parts)) {
                            text(&mut parts, " ");
                        }
                        parts.push(Part::Math {
                            rect: Rect {
                                left: it.x,
                                right: it.x + it.w,
                                top: it.top,
                                bottom: it.bottom,
                            },
                            baseline: ln.y,
                            size: ln.size,
                            line: i,
                        });
                    }
                }
            } else {
                if prev.is_some()
                    && spaced
                    && !ends_space(tail(&parts))
                    && !it.str.chars().next().is_some_and(js_space)
                {
                    text(&mut parts, " ");
                }
                text(&mut parts, &it.str);
            }
            prev = Some(it);
        }
    }
    for part in &mut parts {
        if let Part::Text(s) = part {
            *s = collapse_space(s);
        }
    }
    parts
}

/// `replace(/\s+/g, " ")`.
fn collapse_space(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    let mut space = false;
    for ch in s.chars() {
        if js_space(ch) {
            space = true;
        } else {
            if space {
                out.push(' ');
                space = false;
            }
            out.push(ch);
        }
    }
    if space {
        out.push(' ');
    }
    out
}

/// The text of the parts, with `placeholder` for each formula.
pub fn parts_text(parts: &[Part], placeholder: &str) -> String {
    parts
        .iter()
        .map(|p| p.text().unwrap_or(placeholder))
        .collect()
}

/// Whether a paragraph is worth translating: words, not numbers; not a small label in a figure.
/// `body_size` is the font size of most of the document's text.
pub fn worth_translating(parts: &[Part], block: &Block, body_size: f64) -> bool {
    let text = parts_text(parts, "");
    let visible = js_len(&without_space(&text));
    let letters = LETTER.find_iter(&text).count();
    if letters < 2 || (letters as f64) < 0.5 * visible as f64 {
        return false;
    }
    let words = WORD2.find_iter(&text).count();
    let cjk = text.chars().filter(|&c| is_cjk(c)).count();
    words >= 3 || cjk >= 4 || block.size >= 0.9 * body_size
}

/// How the paragraph's lines sit: centered (titles), justified, or from the left.
pub fn alignment(block: &Block, page_width: f64) -> Align {
    let lines = &block.lines;
    if lines.first().is_some_and(|l| {
        l.items
            .first()
            .is_some_and(|it| BULLET.is_match(it.str.trim_start()))
    }) {
        return Align::Left;
    }
    if lines.len() < 2 {
        let centered = (middle(block.left, block.right) - page_width / 2.0).abs()
            < 0.02 * page_width
            && block.right - block.left < 0.8 * page_width;
        return if centered { Align::Center } else { Align::Left };
    }
    let spread = |values: &mut dyn Iterator<Item = f64>| {
        let (lo, hi) = values.fold((f64::INFINITY, f64::NEG_INFINITY), |(lo, hi), v| {
            (lo.min(v), hi.max(v))
        });
        hi - lo
    };
    let left_spread = spread(&mut lines.iter().map(|l| l.left));
    let center_spread = spread(&mut lines.iter().map(|l| middle(l.left, l.right)));
    if left_spread > block.size && center_spread < 0.5 * block.size {
        return Align::Center;
    }
    // Justified when the lines but the last fill the width; a short block (a name over an
    // address) would have its few words spread out.
    let full = lines[..lines.len() - 1]
        .iter()
        .all(|l| block.right - l.right < block.size);
    if lines.len() >= 3 && full {
        Align::Justify
    } else {
        Align::Left
    }
}

#[cfg(test)]
mod tests {
    //! Paragraph grouping and text normalization from positioned runs.
    use super::*;

    // A page 600 points wide, with 10-point text on lines 12 points apart.
    const PAGE: f64 = 600.0;

    #[derive(Default)]
    struct More {
        size: Option<f64>,
        bold: bool,
        font: Option<&'static str>,
        math: bool,
    }

    /// A piece of text from `x` to `right` on the baseline `y`.
    fn piece(s: &str, x: f64, right: f64, y: f64, more: More) -> Item {
        let size = more.size.unwrap_or(10.0);
        Item {
            str: s.into(),
            x,
            y,
            w: right - x,
            size,
            top: y - 0.8 * size,
            bottom: y + 0.2 * size,
            font: more.font.unwrap_or("f1").into(),
            bold: more.bold,
            italic: false,
            mono: false,
            serif: true,
            math: more.math,
            id: 0,
        }
    }

    fn p(s: &str, x: f64, right: f64, y: f64) -> Item {
        piece(s, x, right, y, More::default())
    }

    /// The texts of the paragraphs found in `items`.
    fn paragraphs(items: Vec<Item>) -> Vec<String> {
        blocks_of(lines_of(items), PAGE)
            .iter()
            .map(|b| parts_text(&parts_of(b), "□"))
            .collect()
    }

    #[test]
    fn short_list_first_line_keeps_a_long_url_continuation() {
        let blocks = blocks_of(
            lines_of(vec![
                p("• See the documentation at", 100.0, 260.0, 100.0),
                p(
                    "https://example.org/a/long/unbreakable/documentation/path",
                    112.0,
                    420.0,
                    112.0,
                ),
            ]),
            612.0,
        );
        assert_eq!(blocks.len(), 1);
        assert_eq!(blocks[0].lines.len(), 2);
    }

    #[test]
    fn lines_of_a_paragraph_are_joined_an_indented_line_starts_the_next() {
        assert_eq!(
            paragraphs(vec![
                p("The first paragraph begins here and", 100.0, 500.0, 100.0),
                p("ends on its second line.", 100.0, 300.0, 112.0),
                p("The second one is indented and runs", 115.0, 500.0, 124.0),
                p("on below it.", 100.0, 200.0, 136.0),
            ]),
            [
                "The first paragraph begins here and ends on its second line.",
                "The second one is indented and runs on below it."
            ]
        );
    }

    #[test]
    fn a_word_hyphenated_at_the_end_of_a_line_is_joined_again() {
        assert_eq!(
            paragraphs(vec![
                p("This is a rather compli-", 100.0, 500.0, 100.0),
                p("cated sentence, a well-", 100.0, 500.0, 112.0),
                p("Known one.", 100.0, 300.0, 124.0),
            ]),
            ["This is a rather complicated sentence, a well- Known one."]
        );
    }

    #[test]
    fn no_space_between_the_lines_of_cjk_text() {
        assert_eq!(
            paragraphs(vec![
                p("吾輩は猫である。名前はまだ", 100.0, 500.0, 100.0),
                p("無い。", 100.0, 160.0, 112.0),
            ]),
            ["吾輩は猫である。名前はまだ無い。"]
        );
    }

    #[test]
    fn a_larger_gap_another_size_or_a_bold_line_starts_a_paragraph() {
        assert_eq!(
            paragraphs(vec![
                piece(
                    "Results",
                    100.0,
                    160.0,
                    100.0,
                    More {
                        bold: true,
                        ..More::default()
                    }
                ),
                p("The text below the heading is", 100.0, 500.0, 112.0),
                p("one paragraph.", 100.0, 200.0, 124.0),
                p("After a gap comes another.", 100.0, 400.0, 150.0),
                piece(
                    "A note in small print.",
                    100.0,
                    300.0,
                    160.0,
                    More {
                        size: Some(7.0),
                        ..More::default()
                    }
                ),
            ]),
            [
                "Results",
                "The text below the heading is one paragraph.",
                "After a gap comes another.",
                "A note in small print."
            ]
        );
    }

    #[test]
    fn columns_side_by_side_are_apart_a_wide_gap_ends_a_line() {
        assert_eq!(
            paragraphs(vec![
                p("Left column, line one", 50.0, 280.0, 100.0),
                p("Right column, line one", 320.0, 550.0, 100.0),
                p("left column, line two.", 50.0, 280.0, 112.0),
                p("right column, line two.", 320.0, 550.0, 112.0),
            ]),
            // In the order of the PDF's text, which a real PDF gives column by column.
            [
                "Left column, line one",
                "Right column, line one",
                "left column, line two.",
                "right column, line two."
            ]
        );
    }

    #[test]
    fn a_centered_paragraph_is_one_its_short_last_line_too() {
        assert_eq!(
            paragraphs(vec![
                p(
                    "Before closing the lantern cabinet lid, check the catches along the edge to",
                    120.0,
                    480.0,
                    100.0
                ),
                p(
                    "make sure the tallest paper models have enough room even with checked ribbons or",
                    121.0,
                    479.0,
                    112.0
                ),
                p("decorative bows.", 260.0, 340.0, 124.0),
            ]),
            [
                "Before closing the lantern cabinet lid, check the catches along the edge to make sure the tallest paper models have enough room even with checked ribbons or decorative bows."
            ]
        );
    }

    #[test]
    fn a_centered_title_of_two_lines_is_one_paragraph_centered() {
        let big = || More {
            size: Some(14.0),
            ..More::default()
        };
        let blocks = blocks_of(
            lines_of(vec![
                piece(
                    "A Title That Is Long Enough to Need",
                    150.0,
                    450.0,
                    100.0,
                    big(),
                ),
                piece("Two Lines", 260.0, 340.0, 116.0, big()),
            ]),
            PAGE,
        );
        assert_eq!(
            (blocks.len(), alignment(&blocks[0], PAGE)),
            (1, Align::Center)
        );
    }

    #[test]
    fn short_centered_lines_below_one_another_stay_apart() {
        assert_eq!(
            paragraphs(vec![
                p("Paper Models", 270.0, 330.0, 100.0),
                p(
                    "Workshop for Paper Lanterns, North Room",
                    190.0,
                    410.0,
                    112.0
                ),
            ]),
            ["Paper Models", "Workshop for Paper Lanterns, North Room"]
        );
    }

    #[test]
    fn numbered_headings_below_one_another_are_each_their_own() {
        assert_eq!(
            paragraphs(vec![
                piece(
                    "3. Large Folding Lanterns",
                    100.0,
                    300.0,
                    100.0,
                    More {
                        bold: true,
                        size: Some(12.0),
                        ..More::default()
                    }
                ),
                piece(
                    "3.1. Foldable Lanterns",
                    100.0,
                    280.0,
                    116.0,
                    More {
                        bold: true,
                        size: Some(11.0),
                        ..More::default()
                    }
                ),
            ]),
            ["3. Large Folding Lanterns", "3.1. Foldable Lanterns"]
        );
    }

    #[test]
    fn a_list_item_starts_a_paragraph() {
        assert_eq!(
            paragraphs(vec![
                p("Two things matter:", 100.0, 300.0, 100.0),
                p("• the first, and", 100.0, 300.0, 112.0),
                p("• the second.", 100.0, 300.0, 124.0),
            ]),
            ["Two things matter:", "• the first, and", "• the second."]
        );
    }

    fn math(font: &'static str) -> More {
        More {
            font: Some(font),
            math: true,
            ..More::default()
        }
    }

    #[test]
    fn a_formula_on_a_line_of_its_own_is_left_out_and_ends_the_paragraph() {
        assert_eq!(
            paragraphs(vec![
                p("We compute the outputs as:", 100.0, 400.0, 100.0),
                piece("QKT", 250.0, 300.0, 118.0, math("f2")),
                p("where the keys are packed too.", 100.0, 400.0, 136.0),
            ]),
            [
                "We compute the outputs as:",
                "where the keys are packed too."
            ]
        );
    }

    #[test]
    fn a_formula_inside_a_paragraph_is_a_part_of_its_own_with_the_digits_beside_it() {
        assert_eq!(
            paragraphs(vec![
                p("values of dimension ", 100.0, 200.0, 100.0),
                piece("d", 200.0, 206.0, 100.0, math("f2")),
                piece(
                    "v",
                    206.0,
                    210.0,
                    102.0,
                    More {
                        size: Some(7.0),
                        ..More::default()
                    }
                ),
                p(" are used in every layer.", 212.0, 400.0, 100.0),
            ]),
            ["values of dimension □ are used in every layer."]
        );
    }

    #[test]
    fn alignments() {
        let blocks = blocks_of(
            lines_of(vec![
                p(
                    "A justified paragraph fills its lines to",
                    100.0,
                    500.0,
                    100.0,
                ),
                p(
                    "the right edge, all of them except for",
                    100.0,
                    500.0,
                    112.0,
                ),
                p("the last.", 100.0, 160.0, 124.0),
            ]),
            PAGE,
        );
        assert_eq!(alignment(&blocks[0], PAGE), Align::Justify, "justified");
        let one = |s: &str, x, right| {
            alignment(
                &blocks_of(lines_of(vec![p(s, x, right, 100.0)]), PAGE)[0],
                PAGE,
            )
        };
        assert_eq!(one("Abstract", 270.0, 330.0), Align::Center, "middle");
        assert_eq!(one("1 Introduction", 100.0, 200.0), Align::Left, "left");
    }

    #[test]
    fn inferred_leading_requires_wrapped_runs() {
        let prose = |count: usize, gap: f64, right: f64| {
            (0..count)
                .map(|i| {
                    p(
                        "本文の行送りを確かめる文章です",
                        if i == 0 { 110.0 } else { 100.0 },
                        right,
                        100.0 + i as f64 * gap,
                    )
                })
                .collect::<Vec<_>>()
        };
        assert_eq!(leading_limit(&[lines_of(prose(19, 19.5, 500.0))]), 1.75);
        assert_eq!(
            leading_limit(&[lines_of(prose(20, 19.5, 500.0))]),
            2.1060000000000003
        );
        assert_eq!(paragraphs(prose(20, 19.5, 500.0)).len(), 1);
        assert_eq!(leading_limit(&[lines_of(prose(20, 25.0, 500.0))]), 2.7);
        assert_eq!(
            leading_limit(&[lines_of(prose(20, 19.5, 180.0))]),
            2.1060000000000003
        );
        assert_eq!(
            leading_limit(&[
                lines_of(prose(11, 19.5, 500.0)),
                lines_of(prose(11, 19.5, 500.0))
            ]),
            2.1060000000000003
        );
        let mut items = prose(20, 19.5, 500.0);
        items.extend([
            p("• 箇条書きは別の段落です", 100.0, 500.0, 490.0),
            piece(
                "見出しを別の段落にする",
                100.0,
                500.0,
                509.5,
                More {
                    bold: true,
                    ..More::default()
                },
            ),
            p("本文へ戻る最初の行です", 100.0, 500.0, 529.0),
            p("空きを置いて始まる段落です", 100.0, 500.0, 556.0),
        ]);
        assert_eq!(paragraphs(items).len(), 5);
        let headings = lines_of(vec![
            p("短い見出しを分離する", 100.0, 280.0, 100.0),
            p("次の小見出しも分離する", 100.0, 260.0, 119.5),
        ]);
        assert_eq!(blocks_with_leading(headings, PAGE, 2.4375).len(), 2);
    }

    // Test line spacing and paragraph boundaries directly with positioned text.
    #[test]
    fn a_single_list_item_does_not_absorb_a_differently_indented_paragraph() {
        for (marker, first_left, next_left) in [("3. ", 90.0, 72.0), ("・", 72.0, 84.0)] {
            let lines = lines_of(vec![
                piece(
                    &format!("{marker}短い項目"),
                    first_left,
                    250.0,
                    72.0,
                    More::default(),
                ),
                piece(
                    "字下げの異なる次の本文は独立した段落です。",
                    next_left,
                    550.0,
                    87.0,
                    More::default(),
                ),
            ]);
            assert_eq!(blocks_with_leading(lines, 612.0, 1.75).len(), 2);
        }
    }

    #[test]
    fn layouts_keep_items_headings_tables_and_slides_separate() {
        let line = |text: &str, x, right, y, size| {
            piece(
                text,
                x,
                right,
                y,
                More {
                    size: Some(size),
                    ..More::default()
                },
            )
        };
        let body = |n: usize, y: f64| {
            lines_of(
                (0..n)
                    .map(|i| {
                        line(
                            if i + 1 == n {
                                "段落の最後の行です。"
                            } else {
                                "本文の行送りを確かめる文章ですが段落の途中なので続きます"
                            },
                            if i == 0 { 84.0 } else { 72.0 },
                            if i + 1 == n { 252.0 } else { 552.0 },
                            y + i as f64 * 23.4,
                            12.0,
                        )
                    })
                    .collect(),
            )
        };
        let rhythm = leading_limit(&[body(12, 72.0), body(12, 72.0)]);
        assert!((rhythm / 1.08 - 1.95).abs() < 1e-9);
        for marker in [
            "・", "⚫", "●", "○", "■", "□", "◆", "◇", "▪", "▶", "➢", "※", "1. ", "(1) ", "①",
        ] {
            let items = lines_of(
                (0..3)
                    .map(|i| {
                        line(
                            &format!("{marker}独立した箇条書きの文章が続きます。"),
                            72.0,
                            552.0,
                            72.0 + i as f64 * 23.4,
                            12.0,
                        )
                    })
                    .collect(),
            );
            assert_eq!(
                blocks_with_leading(items, 612.0, rhythm).len(),
                3,
                "{marker}"
            );
        }
        let mut hanging = Vec::new();
        for i in 0..3 {
            let y = 72.0 + i as f64 * 42.0;
            hanging.push(line(
                "⚫ 項目の一行目の文章は長く続いて次の行に回ります、そして",
                72.0,
                552.0,
                y,
                12.0,
            ));
            hanging.push(line(
                "二行目で項目が終わります。",
                86.4,
                312.0,
                y + 18.0,
                12.0,
            ));
        }
        let blocks = blocks_with_leading(lines_of(hanging), 612.0, rhythm);
        assert_eq!(
            blocks.iter().map(|b| b.lines.len()).collect::<Vec<_>>(),
            [2, 2, 2]
        );
        for gap in [23.4, 26.4] {
            let mut heading = vec![line("同じ太さの見出し", 72.0, 228.0, 72.0, 12.0)];
            let mut prose = body(5, 72.0 + gap);
            // No first-line indent: separation must come from the short heading/right edge.
            prose[0].items[0].x = 72.0;
            prose[0].items[0].w = 480.0;
            heading.extend(prose.into_iter().flat_map(|l| l.items));
            let blocks = blocks_with_leading(lines_of(heading), 612.0, rhythm);
            assert_eq!(
                blocks.iter().map(|b| b.lines.len()).collect::<Vec<_>>(),
                [1, 5]
            );
        }
        let rows = lines_of(
            [
                "展示模型：小箱に飾っている状態",
                "紙製灯籠：青と白の縞模様を付ける",
                "折り模様：色の異なる紙帯で縁取る",
                "展示用棚：奥の壁面の横桟に掛ける",
            ]
            .iter()
            .enumerate()
            .map(|(i, t)| line(t, 72.0, 432.0, 72.0 + i as f64 * 24.0, 12.0))
            .collect(),
        );
        assert_eq!(blocks_with_leading(rows, 612.0, rhythm).len(), 4);
        let slide = || {
            lines_of(
                (0..10)
                    .map(|i| {
                        line(
                            &format!("デジタル人材の育成を推進する施策の第{i}項目"),
                            40.0,
                            580.0,
                            60.0 + i as f64 * 39.6,
                            18.0,
                        )
                    })
                    .collect(),
            )
        };
        let slide_rhythm = leading_limit(&[slide(), slide(), slide()]);
        assert_eq!(slide_rhythm, 1.75);
        assert_eq!(blocks_with_leading(slide(), 960.0, slide_rhythm).len(), 10);
        let narrow = lines_of(
            (0..25)
                .map(|i| {
                    line(
                        "狭い段組の本文が続きます、まだ続く",
                        if i == 0 { 84.0 } else { 72.0 },
                        276.0,
                        72.0 + i as f64 * 23.4,
                        12.0,
                    )
                })
                .collect(),
        );
        let narrow_rhythm = leading_limit(std::slice::from_ref(&narrow));
        assert_eq!(
            blocks_with_leading(narrow, 612.0, narrow_rhythm)[0]
                .lines
                .len(),
            25
        );
        // Repeated two-line boxes must never teach the document an inter-item rhythm.
        let pairs: Vec<_> = (0..20).map(|_| body(2, 72.0)).collect();
        assert_eq!(leading_limit(&pairs), 1.75);
    }

    #[test]
    fn short_line_ends_a_block_before_its_first_join_with_extra_leading() {
        let mut items = (0..22)
            .map(|i| {
                p(
                    "折り返された本文の行をここに配置します",
                    if i == 0 { 110.0 } else { 100.0 },
                    500.0,
                    100.0 + i as f64 * 19.5,
                )
            })
            .collect::<Vec<_>>();
        items.push(p("短い最終行です。", 100.0, 260.0, 529.0));
        items.push(p("別の注記の行がここから始まります", 100.0, 500.0, 548.5));
        let blocks = blocks_of(lines_of(items), PAGE);
        assert_eq!(
            blocks.iter().map(|b| b.lines.len()).collect::<Vec<_>>(),
            [23, 1]
        );
    }

    #[test]
    fn citation_years_are_not_numbered_list_markers() {
        for year in ["2018) used the same model.", "(2022) is a reference."] {
            assert_eq!(
                paragraphs(vec![
                    p("The authors in this reference", 100.0, 500.0, 100.0),
                    p(year, 100.0, 300.0, 112.0),
                ])
                .len(),
                1
            );
        }
    }

    #[test]
    fn cjk_boundaries_match_javascript() {
        for (left, right, spaced) in [
            ("한국어 문장을", "다음 줄에서 읽습니다", true),
            ("가", "나", true),
            ("ㄱㅏ", "ㄴㅏ", true),
            ("가", "나", true),
            ("𠮷", "𠮷", false),
            ("﨑", "丽", false),
            ("⼀", "⺀", false),
            ("ｶｰ", "カナ", false),
            ("。", "Ａ", false),
            ("", "", true),
            ("ﾠ", "ﾡ", true),
            ("𰀀", "𲎰", false),
        ] {
            assert_eq!(
                paragraphs(vec![
                    p(left, 100.0, 500.0, 100.0),
                    p(right, 100.0, 300.0, 112.0)
                ]),
                [format!("{left}{}{right}", if spaced { " " } else { "" })]
            );
        }
    }

    #[test]
    fn worth_translating_cases() {
        let worth = |s: &str, size: f64| {
            let blocks = blocks_of(
                lines_of(vec![piece(
                    s,
                    100.0,
                    300.0,
                    100.0,
                    More {
                        size: Some(size),
                        ..More::default()
                    },
                )]),
                PAGE,
            );
            worth_translating(&parts_of(&blocks[0]), &blocks[0], 10.0)
        };
        assert!(worth("The model is trained.", 10.0), "a sentence");
        assert!(!worth("0.25 1.50 3.75", 10.0), "not numbers");
        assert!(!worth("Linear", 6.0), "not a small label of a figure");
        assert!(worth("Abstract", 10.0), "a heading in the text's size");
        assert!(worth("한국어 문장을 번역합니다", 6.0), "Korean words");
        assert!(worth("𠮷𠮷𠮷𠮷", 6.0), "supplementary CJK");
        assert!(worth("概要と背景", 6.0), "Japanese");
    }

    fn kind(mono: bool, serif: bool, bold: bool, italic: bool) -> FontKind {
        FontKind {
            mono,
            serif,
            bold,
            italic,
        }
    }

    #[test]
    fn font_kinds() {
        let none = FontFlags::default();
        assert_eq!(
            font_kind("ABCDEF+NimbusRomNo9L-Regu", none),
            kind(false, true, false, false),
            "Times is serif"
        );
        assert_eq!(
            font_kind("CYUPTR+CMBX10", none),
            kind(false, true, true, false),
            "Computer Modern bold"
        );
        assert_eq!(
            font_kind(
                "AAAAAA+NotoSansCJKjp-Regular",
                FontFlags {
                    serif: Some(true),
                    ..none
                }
            ),
            kind(false, false, false, false),
            "a sans CJK font is not serif, whatever it says"
        );
        assert_eq!(
            font_kind("MS-Mincho", none),
            kind(false, true, false, false),
            "Mincho is serif"
        );
        assert_eq!(
            font_kind("JQUDPS+NimbusMonL-Regu", none),
            kind(true, false, false, false),
            "monospace by its name"
        );
        assert!(font_kind("XEXHSJ+SFTT1000", none).mono, "TeX's typewriter");
        let mono = FontFlags {
            mono: Some(true),
            ..none
        };
        assert!(
            font_kind("ABCDEF+TT12", mono).mono,
            "monospace by the PDF's flag, for an unknown name"
        );
        assert!(
            !font_kind("WMCLTX+CMBX10", mono).mono,
            "a Roman font is not monospace, whatever it says"
        );
        assert_eq!(
            font_kind(
                "KGMNOB+TT93o00",
                FontFlags {
                    serif: Some(true),
                    bold: Some(true),
                    ..none
                }
            ),
            kind(false, true, true, false),
            "an unknown name follows the PDF's flags"
        );
        assert_eq!(
            font_kind("Arial-BoldItalicMT", none),
            kind(false, false, true, true),
            "italic and bold by name"
        );
        // Recognize monospace names without mistaking Monotype for a monospace family.
        assert!(!font_kind("Monotype-Sorts", none).mono);
        assert!(font_kind("Monotype-Mono", none).mono);
    }

    #[test]
    fn math_by_font_or_symbol() {
        assert!(is_math("YYBXCM+CMMI10", "x"), "by the font");
        assert!(is_math("Arial", "≤"), "by the symbol");
        assert!(!is_math("Arial", "alpha"), "not a word");
    }

    #[test]
    fn normalizes_radicals_and_ligatures_only() {
        assert_eq!(normalize("⽣まれて⾒た⼈"), "生まれて見た人");
        assert_eq!(normalize("ﬁnal ﬂow"), "final flow");
        assert_eq!(normalize("生 ① ｱ ㌔"), "生 ① ｱ ㌔");
    }

    #[test]
    fn to_fixed_rounds_ties_up() {
        assert_eq!(fixed1(10.25), "10.3");
        assert_eq!(fixed1(9.96), "10.0");
        assert_eq!(fixed1(7.0), "7.0");
    }

    // Exercise JavaScript-compatible rounding at font size boundaries.

    #[test]
    fn to_fixed_rounds_the_value_held_not_the_decimal_written() {
        for (x, want) in [
            (8.35, "8.3"),
            (8.34, "8.3"),
            (8.25, "8.3"),
            (8.45, "8.4"),
            (1.45, "1.4"),
            (1.05, "1.1"),
            (0.25, "0.3"),
            (0.75, "0.8"),
            (12.75, "12.8"),
            (2.5, "2.5"),
        ] {
            assert_eq!(fixed1(x), want, "{x}");
        }
    }

    #[test]
    fn a_line_s_main_font_is_counted_by_its_size_as_javascript_rounds_it() {
        let line = |small: f64, large: f64| {
            let lines = lines_of(vec![
                piece(
                    "a",
                    100.0,
                    105.0,
                    100.0,
                    More {
                        size: Some(small),
                        ..More::default()
                    },
                ),
                piece(
                    "longword",
                    105.0,
                    145.0,
                    100.0,
                    More {
                        size: Some(large),
                        bold: true,
                        ..More::default()
                    },
                ),
            ]);
            assert_eq!(lines.len(), 1);
            (lines[0].size, lines[0].bold)
        };
        // 8.34 and 8.35 are both "8.3": one font, whose first piece gives the line its size.
        assert_eq!(line(8.34, 8.35), (8.34, false));
        // 10.2 is "10.2" and 10.25 "10.3": the bold piece has more of the line.
        assert_eq!(line(10.2, 10.25), (10.25, true));
    }

    // Exercise JavaScript-compatible digit, whitespace and case matching.

    #[test]
    fn full_width_digits_are_not_a_numbered_heading() {
        let bold = || More {
            bold: true,
            ..More::default()
        };
        // JavaScript's `\d` is ASCII: `１` does not start a heading, so the bold lines join.
        assert_eq!(
            paragraphs(vec![
                piece("１ 概要", 100.0, 300.0, 100.0, bold()),
                piece("２ Ａ方式について", 100.0, 300.0, 112.0, bold()),
            ]),
            ["１ 概要２ Ａ方式について"]
        );
        assert_eq!(
            paragraphs(vec![
                piece("1 Overview", 100.0, 300.0, 100.0, bold()),
                piece("2 The A method", 100.0, 300.0, 112.0, bold()),
            ]),
            ["1 Overview", "2 The A method"]
        );
    }

    #[test]
    fn next_line_is_not_white_space() {
        // U+0085 is white space to Rust, not to JavaScript's `\s`.
        assert_eq!(
            paragraphs(vec![
                p("a\u{85}b", 100.0, 300.0, 100.0),
                p("c", 100.0, 300.0, 112.0),
            ]),
            ["a\u{85}b c"]
        );
        assert_eq!(
            paragraphs(vec![
                p("one\u{85}", 100.0, 130.0, 100.0),
                p("two", 140.0, 160.0, 100.0),
            ]),
            ["one\u{85} two"]
        );
    }

    #[test]
    fn font_names_fold_ascii_case_only() {
        let serif = FontFlags {
            serif: Some(true),
            ..FontFlags::default()
        };
        // `/…/i` without `u`: `ſ` is not `s`, nor `K` (Kelvin) `k`, nor `１` a digit.
        assert_eq!(font_kind("ſans", serif), kind(false, true, false, false));
        assert_eq!(
            font_kind("\u{212A}ourier", serif),
            kind(false, true, false, false)
        );
        assert_eq!(font_kind("CMTT１", serif), kind(false, true, false, false));
        assert_eq!(font_kind("CMTT1", serif), kind(true, false, false, false));
        // `Hei` before a character that is not an ASCII letter (`ſ` is not one).
        assert_eq!(font_kind("Heiſ", serif), kind(false, false, false, false));
        assert_eq!(font_kind("sans", serif), kind(false, false, false, false));
    }

    // Paragraph boundaries around marks, list items, notes and emphasis.

    /// A piece whose formula flag is set as the extraction sets it, from its font and text.
    fn typed(s: &str, x: f64, right: f64, y: f64, font: &'static str, size: f64) -> Item {
        piece(
            s,
            x,
            right,
            y,
            More {
                size: Some(size),
                font: Some(font),
                math: is_math(font, s),
                ..More::default()
            },
        )
    }

    #[test]
    fn footnote_stars_and_symbol_font_bullets_are_marks_not_formulas() {
        assert!(!is_math("ABCDEF+CMSY9", "∗"));
        assert!(!is_math("ABCDEF+CMSY7", "1,∗"));
        assert!(!is_math("Symbol", &normalize("\u{f0b7}")));
        assert!(is_math("ABCDEF+CMSY10", "∗x"), "a mark with a variable");
        assert!(is_math("ABCDEF+CMMI10", "x"));
        assert_eq!(normalize("\u{f0b7} \u{f0a7}"), "• ▪");
    }

    #[test]
    fn arrows_and_greek_letters_between_words_are_prose_unlike_formulas() {
        assert_eq!(
            paragraphs(vec![
                p("Text", 100.0, 125.0, 100.0),
                typed("→", 128.0, 138.0, 100.0, "Arial", 10.0),
                p(".rtf, .txt and more", 141.0, 230.0, 100.0),
            ]),
            ["Text → .rtf, .txt and more"]
        );
        assert_eq!(
            paragraphs(vec![
                p("the", 100.0, 115.0, 100.0),
                typed("α", 118.0, 124.0, 100.0, "TimesNewRoman", 10.0),
                p("parameter is set", 127.0, 200.0, 100.0),
            ]),
            ["the α parameter is set"]
        );
        // A variable of a formula font, and an arrow between formula pieces, stay formulas.
        assert_eq!(
            paragraphs(vec![
                p("where", 100.0, 125.0, 100.0),
                typed("α", 128.0, 134.0, 100.0, "ABCDEF+CMMI10", 10.0),
                p("is the rate", 137.0, 190.0, 100.0),
            ]),
            ["where □ is the rate"]
        );
        assert_eq!(
            paragraphs(vec![
                p("We map", 100.0, 135.0, 100.0),
                typed("x", 138.0, 143.0, 100.0, "ABCDEF+CMMI10", 10.0),
                typed("→", 146.0, 156.0, 100.0, "ABCDEF+CMSY10", 10.0),
                typed("y", 159.0, 164.0, 100.0, "ABCDEF+CMMI10", 10.0),
                p("with it", 167.0, 200.0, 100.0),
            ]),
            ["We map □ with it"]
        );
    }

    /// Pieces set one after another from `x` on the baseline `y`, each half its size wide a
    /// character (a CJK character its size), half a point apart. A piece smaller than 10
    /// points is a subscript, 2 points below the baseline.
    fn set(pieces: &[(&str, &'static str, f64)], x: f64, y: f64) -> Vec<Item> {
        let mut x = x;
        pieces
            .iter()
            .map(|&(s, font, size)| {
                let w: f64 = s
                    .chars()
                    .map(|c| if is_cjk(c) { size } else { 0.5 * size })
                    .sum();
                let base = if size < 10.0 { y + 2.0 } else { y };
                let it = typed(s, x, x + w, base, font, size);
                x += w + 0.5;
                it
            })
            .collect()
    }

    /// What becomes of `items`: `as drawn` for each line left out of the paragraphs (a
    /// formula on a line of its own), then the paragraphs' texts, a formula piece as `□`.
    fn outcome(items: Vec<Item>) -> Vec<String> {
        let lines = lines_of(items);
        let mut out: Vec<String> = (lines.iter().filter(|l| left_out(l)))
            .map(|_| "as drawn".to_owned())
            .collect();
        out.extend(
            blocks_of(lines, PAGE)
                .iter()
                .map(|b| parts_text(&parts_of(b), "□")),
        );
        out
    }

    // A standalone formula drawn in a text font with a
    // sign of a symbol font, has words too (`NaCl`), but is no sentence: it stays a formula.
    #[test]
    fn formulas_of_a_text_font_on_lines_of_their_own_stay_formulas() {
        const T: &str = "TimesNewRoman";
        const C: &str = "Courier";
        const S: &str = "Symbol";
        // A formula of mathematics.
        assert_eq!(
            outcome(set(
                &[("sin x ", T, 10.0), ("×", S, 10.0), (" cos y", T, 10.0)],
                100.0,
                100.0
            )),
            ["sin x □ cos y"]
        );
        // Chemical equations, with the arrow `→` of the symbol font, `⟶` of
        // a formula font, `=` of the symbol font, and `⇌` of a text font.
        assert_eq!(
            outcome(set(
                &[
                    ("NaCl + AgNO3 ", C, 12.0),
                    ("→", S, 12.0),
                    (" AgCl + NaNO3", C, 12.0)
                ],
                120.0,
                100.0
            )),
            ["NaCl + AgNO3 □ AgCl + NaNO3"]
        );
        assert_eq!(
            outcome(set(
                &[
                    ("2H2 + O2 ", T, 10.0),
                    ("⟶", "CambriaMath", 10.0),
                    (" 2H2O", T, 10.0)
                ],
                100.0,
                100.0
            )),
            ["as drawn"]
        );
        assert_eq!(
            outcome(set(
                &[
                    ("CH4 + 2O2 ", T, 10.0),
                    ("=", S, 10.0),
                    (" CO2 + 2H2O", T, 10.0)
                ],
                100.0,
                100.0
            )),
            ["as drawn"]
        );
        assert_eq!(
            outcome(set(
                &[
                    ("CH3COOH ", T, 10.0),
                    ("⇌", "Arial", 10.0),
                    (" CH3COO− + H+", T, 10.0)
                ],
                100.0,
                100.0
            )),
            ["CH3COOH □ CH3COO− + H+"]
        );
        // A calculation with units.
        assert_eq!(
            outcome(set(
                &[
                    ("5 km/h ", T, 10.0),
                    ("×", S, 10.0),
                    (" 2 h = 10 km", T, 10.0)
                ],
                100.0,
                100.0
            )),
            ["as drawn"]
        );
        assert_eq!(
            outcome(set(
                &[
                    ("1.2 kPa ", T, 10.0),
                    ("−", "Arial", 10.0),
                    (" 0.4 kPa = 0.8 kPa", T, 10.0)
                ],
                100.0,
                100.0
            )),
            ["1.2 kPa □ 0.4 kPa = 0.8 kPa"]
        );
        // Subscripts.
        assert_eq!(
            outcome(set(
                &[
                    ("NaHCO", T, 10.0),
                    ("3", T, 7.0),
                    (" + HCl ", T, 10.0),
                    ("→", S, 10.0),
                    (" NaCl + H", T, 10.0),
                    ("2", T, 7.0),
                    ("O + CO", T, 10.0),
                    ("2", T, 7.0),
                ],
                100.0,
                100.0
            )),
            ["NaHCO3 + HCl □ NaCl + H2O + CO2"]
        );
        // The equation's number at the right: a paragraph of its own.
        let mut numbered = set(
            &[
                ("NaCl + AgNO3 ", C, 12.0),
                ("→", S, 12.0),
                (" AgCl + NaNO3", C, 12.0),
            ],
            120.0,
            100.0,
        );
        numbered.push(p("(1)", 500.0, 515.0, 100.0));
        assert_eq!(outcome(numbered), ["NaCl + AgNO3 □ AgCl + NaNO3", "(1)"]);
        // Lines of formulas one below another, and a bullet before one.
        let mut lines = set(
            &[("Fe + S ", T, 10.0), ("→", S, 10.0), (" FeS", T, 10.0)],
            100.0,
            100.0,
        );
        lines.extend(set(
            &[
                ("2Fe + 3Cl2 ", T, 10.0),
                ("→", S, 10.0),
                (" 2FeCl3", T, 10.0),
            ],
            100.0,
            112.0,
        ));
        lines.extend(set(
            &[
                ("• Zn + 2HCl ", T, 10.0),
                ("→", S, 10.0),
                (" ZnCl2 + H2", T, 10.0),
            ],
            100.0,
            124.0,
        ));
        let found = outcome(lines);
        let kept = |t: &String| t.matches('□').count() + usize::from(t == "as drawn");
        assert!(found.iter().all(|t| kept(t) > 0), "{found:?}");
        assert_eq!(found.iter().map(kept).sum::<usize>(), 3, "{found:?}");
        // A reaction named in a sentence, its formulas beside the arrow: a formula inside
        // the sentence, which stays as drawn.
        assert_eq!(
            outcome(set(
                &[
                    ("The silver nitrate solution reacts as AgNO3 ", T, 10.0),
                    ("→", S, 10.0),
                    (" AgCl when salt is added.", T, 10.0)
                ],
                60.0,
                100.0
            )),
            ["The silver nitrate solution reacts as AgNO3 □ AgCl when salt is added."]
        );
    }

    #[test]
    fn signs_inside_sentences_are_prose_but_not_beside_a_variable() {
        const T: &str = "TimesNewRoman";
        const M: &str = "MS-Mincho";
        const S: &str = "Symbol";
        assert_eq!(
            outcome(set(
                &[
                    ("温度が 25 ", M, 10.0),
                    ("→", S, 10.0),
                    (" 30 ℃ に上がる", M, 10.0)
                ],
                100.0,
                100.0
            )),
            ["温度が 25 → 30 ℃ に上がる"]
        );
        assert_eq!(
            outcome(set(
                &[
                    ("The temperature rises from 25 ", T, 10.0),
                    ("→", S, 10.0),
                    (" 30 °C during the day.", T, 10.0)
                ],
                60.0,
                100.0
            )),
            ["The temperature rises from 25 → 30 °C during the day."]
        );
        // An arrow drawn in the text's font is a part of its piece, never a formula.
        assert_eq!(
            outcome(set(&[("A → B の順に進む", M, 10.0)], 100.0, 100.0)),
            ["A → B の順に進む"]
        );
        // A symbol font's arrow between single letters: they read as variables (`x → y`),
        // and the line stays as drawn, as before.
        assert_eq!(
            outcome(set(
                &[("A ", M, 10.0), ("→", S, 10.0), (" B の順に進む", M, 10.0)],
                100.0,
                100.0
            )),
            ["A □ B の順に進む"]
        );
        // A plus sign of the text's font is no formula; one of a symbol font is an operator of
        // formulas (not one of the signs of prose), as before.
        assert_eq!(
            outcome(set(
                &[("売上 + 利益の合計で評価する。", M, 10.0)],
                100.0,
                100.0
            )),
            ["売上 + 利益の合計で評価する。"]
        );
        assert_eq!(
            outcome(set(
                &[
                    ("売上 ", M, 10.0),
                    ("+", S, 10.0),
                    (" 利益の合計で評価する。", M, 10.0)
                ],
                100.0,
                100.0
            )),
            ["売上 □ 利益の合計で評価する。"]
        );
        // A bullet and a footnote mark of a symbol font are marks.
        assert_eq!(
            outcome(set(
                &[
                    ("•", S, 10.0),
                    (" Text ", T, 10.0),
                    ("→", S, 10.0),
                    (" .rtf, .txt;", T, 10.0)
                ],
                100.0,
                100.0
            )),
            ["• Text → .rtf, .txt;"]
        );
        assert_eq!(
            outcome(set(
                &[
                    ("∗", "ABCDEF+CMSY6", 10.0),
                    ("Work performed while at the laboratory.", T, 10.0)
                ],
                100.0,
                100.0
            )),
            ["∗Work performed while at the laboratory."]
        );
    }

    #[test]
    fn words_of_formulas_are_told_from_words_of_prose() {
        for t in [
            "NaCl", "AgNO3", "AgCl", "H2O", "x1", "km", "kPa", "mol", "sin", "log", "max",
        ] {
            assert_eq!(token_kind(t), Token::Formula, "{t}");
        }
        for t in [
            "Text",
            "tokens",
            "is",
            "my",
            "csv",
            "Tabellen",
            "JavaScript",
            "WordPiece",
        ] {
            assert_eq!(token_kind(t), Token::Word, "{t}");
        }
        for t in ["x", "A", "PDF", "KOH", "In", "Na"] {
            assert_eq!(token_kind(t), Token::Either, "{t}");
        }
        assert_eq!(token_kind("512"), Token::Number);
    }

    #[test]
    fn a_footnote_or_an_item_keeps_its_lines_after_its_mark() {
        // ∗ of TeX's symbol font, raised and small, then the note; its next line goes on from
        // the left margin below the mark.
        assert_eq!(
            paragraphs(vec![
                typed("∗", 120.0, 124.0, 98.0, "ABCDEF+CMSY6", 6.5),
                p(
                    "Side cabinet label. Folded lanterns mark shelves of paper and",
                    124.0,
                    500.0,
                    100.0
                ),
                p("the edges of the little displays.", 108.0, 300.0, 111.0),
            ]),
            [
                "∗Side cabinet label. Folded lanterns mark shelves of paper and the edges of the little displays."
            ]
        );
        // A full first line that ends its sentence: the line to its left is a paragraph.
        assert_eq!(
            paragraphs(vec![
                typed("∗", 120.0, 124.0, 98.0, "ABCDEF+CMSY6", 6.5),
                p(
                    "Each paper lantern sits on a folded sheet of carton.",
                    124.0,
                    500.0,
                    100.0
                ),
                p(
                    "The next paragraph begins at the margin.",
                    108.0,
                    300.0,
                    111.0
                ),
            ])
            .len(),
            2
        );
        // A bullet drawn by the Symbol font, its item's lines hanging below its text.
        assert_eq!(
            paragraphs(vec![
                // The extraction bridges the gap after a mark with white space.
                typed(&normalize("\u{f0b7}"), 72.0, 77.0, 100.0, "Symbol", 10.0),
                p(" ", 77.0, 90.0, 100.0),
                p(
                    "les grands tiroirs remplis de cordons pourpres, les étagères de",
                    90.0,
                    541.0,
                    100.0
                ),
                p("papier et les bobines de fil décorées.", 90.0, 400.0, 113.0),
                typed(&normalize("\u{f0b7}"), 72.0, 77.0, 126.0, "Symbol", 10.0),
                p(" ", 77.0, 90.0, 126.0),
                p("les boîtes de carton fin.", 90.0, 300.0, 126.0),
            ]),
            [
                "• les grands tiroirs remplis de cordons pourpres, les étagères de papier et les bobines de fil décorées.",
                "• les boîtes de carton fin."
            ]
        );
    }

    #[test]
    fn numbered_items_of_two_pieces_split_and_their_lines_join() {
        assert_eq!(
            paragraphs(vec![
                p("1.", 32.0, 40.0, 100.0),
                p("Allir eiga körfu til geymsluna.", 46.0, 200.0, 100.0),
                p("2.", 32.0, 40.0, 111.0),
                p(
                    "Enginn má að óþörfu færa lampana eða rauðu bönd",
                    46.0,
                    300.0,
                    111.0
                ),
                p("til þess að rýma um góða borðið.", 46.0, 200.0, 122.0),
            ]),
            [
                "1. Allir eiga körfu til geymsluna.",
                "2. Enginn má að óþörfu færa lampana eða rauðu bönd til þess að rýma um góða borðið."
            ]
        );
    }

    #[test]
    fn a_note_goes_on_under_the_text_after_its_label_but_a_table_key_does_not() {
        assert_eq!(
            paragraphs(vec![
                p("(注) 1.", 30.0, 60.0, 100.0),
                p(
                    "ＬＥＤの光量は、目安値２％程度の増加となっており、紙製灯籠を",
                    66.0,
                    500.0,
                    100.0
                ),
                p("置く右側でも同程度となっている。", 66.0, 300.0, 112.0),
            ]),
            [
                "(注) 1. ＬＥＤの光量は、目安値２％程度の増加となっており、紙製灯籠を置く右側でも同程度となっている。"
            ]
        );
        assert_eq!(
            paragraphs(vec![
                p("ESC", 30.0, 50.0, 100.0),
                p(
                    "Drückt den Hebel ab und schiebt die Ablage zurück über den",
                    200.0,
                    500.0,
                    100.0
                ),
                p("beweglichen Sockel.", 200.0, 300.0, 112.0),
            ])
            .len(),
            2
        );
    }

    #[test]
    fn a_reference_hangs_its_lines_but_a_finished_line_ends_its_paragraph() {
        assert_eq!(
            paragraphs(vec![
                p(
                    "Ivo Quen, Neri Vox, and Orel Xi. 2018b. Micro-",
                    72.0,
                    290.0,
                    100.0
                ),
                p(
                    "folded paper lanterns with reversible shade panels",
                    82.0,
                    290.0,
                    111.0
                ),
                p("for indoor model displays.", 82.0, 200.0, 122.0),
                p(
                    "Nilo Vexworth, Ostelmira Quoll, and Amelin T Wex-",
                    72.0,
                    290.0,
                    133.0
                ),
            ]),
            [
                "Ivo Quen, Neri Vox, and Orel Xi. 2018b. Microfolded paper lanterns with reversible shade panels for indoor model displays.",
                "Nilo Vexworth, Ostelmira Quoll, and Amelin T Wex-"
            ]
        );
        // A title over a narrower subtitle centered under it: two paragraphs.
        let title = |s: &str, x: f64, right: f64, y: f64, size: f64| {
            piece(
                s,
                x,
                right,
                y,
                More {
                    size: Some(size),
                    ..More::default()
                },
            )
        };
        assert_eq!(
            paragraphs(vec![
                title("GUIDE DU PLIAGE-PAPIER", 128.8, 468.4, 373.0, 28.0),
                title("RÉUNION SEPTEMBRE 2017", 151.3, 446.3, 409.0, 26.0),
            ])
            .len(),
            2
        );
        // Centered prose of one size goes on.
        assert_eq!(
            paragraphs(vec![
                title(
                    "Lítið hús er hvítt með lága hurð og rúður að",
                    100.0,
                    300.0,
                    100.0,
                    8.0
                ),
                title("sunnan og grænan vegg.", 115.0, 285.0, 110.0, 8.0),
            ])
            .len(),
            1
        );
        assert_eq!(
            paragraphs(vec![
                p(
                    "A full first line that ends with a full stop here.",
                    72.0,
                    290.0,
                    100.0
                ),
                p(
                    "An indented new paragraph starts below it",
                    83.0,
                    290.0,
                    111.0
                ),
            ])
            .len(),
            2
        );
    }

    #[test]
    fn bold_words_inside_a_sentence_keep_it_whole_but_a_heading_stays_apart() {
        let bold = || More {
            bold: true,
            ..More::default()
        };
        assert_eq!(
            paragraphs(vec![
                piece(
                    "Paper, Cardboard, Strings, and Display Ribbons (PCSD)",
                    100.0,
                    400.0,
                    100.0,
                    bold()
                ),
                p("are distributed across all the", 403.0, 500.0, 100.0),
                p("newly painted display shelves.", 100.0, 300.0, 112.0),
            ]),
            [
                "Paper, Cardboard, Strings, and Display Ribbons (PCSD) are distributed across all the newly painted display shelves."
            ]
        );
        assert_eq!(
            paragraphs(vec![
                piece("3.1 Results", 100.0, 160.0, 100.0, bold()),
                p("(draft)", 163.0, 200.0, 100.0),
                p(
                    "The text below the heading goes on to the",
                    100.0,
                    500.0,
                    112.0
                ),
                p("next line.", 100.0, 200.0, 124.0),
            ])
            .len(),
            2
        );
    }

    #[test]
    fn entries_of_a_schedule_each_beginning_with_a_date_stay_apart() {
        // Right-aligned dates start farther left when they have more digits.
        assert_eq!(
            paragraphs(vec![
                p(
                    "5/27：第19回「ともしび館での模型展示に合わせた照明調整について」（紙灯会）",
                    33.0,
                    500.0,
                    100.0
                ),
                p(
                    "8/25：第20回「折り紙模型展示DXの連絡帳」（木の棚班、紙ほたる工作室）",
                    33.0,
                    495.0,
                    114.0
                ),
                p(
                    "10/27：第21回「紙箱を使った模型整理テクニックのご案内」（模型班）",
                    24.0,
                    498.0,
                    128.0
                ),
                p(
                    "11/27：第22回「布製見本ラベルを使った展示棚の飾り方」（折り星班）",
                    24.0,
                    497.0,
                    142.0
                ),
            ])
            .len(),
            4
        );
        // A paragraph whose wrapped line begins with a year, as its first line does: one.
        assert_eq!(
            paragraphs(vec![
                p(
                    "2026年度の予算は前年度から増加し、新たな事業にも重点的に配分されたが、",
                    33.0,
                    500.0,
                    100.0
                ),
                p("2027年度も同じ水準を保つ見込みである。", 33.0, 300.0, 114.0),
            ])
            .len(),
            1
        );
    }

    #[test]
    fn keys_of_entries_are_dates_times_and_ordinals() {
        for t in [
            "5/27：第19回",
            "2026/05/27 開催",
            "2026-05-27 Opening",
            "27.05.2026 Sitzung",
        ] {
            assert_eq!(key_of(t), Some(Key::Date), "{t}");
        }
        for t in ["10月27日 開催", "令和7年度 実績", "2024年"] {
            assert_eq!(key_of(t), Some(Key::Date), "{t}");
        }
        assert_eq!(key_of("10:30 Keynote"), Some(Key::Time));
        assert_eq!(key_of("第21回「クマ」"), Some(Key::Ordinal));
        for t in [
            "3.5 million people",
            "2019 was a year",
            "100 people",
            "12345/6",
        ] {
            assert_eq!(key_of(t), None, "{t}");
        }
    }

    #[test]
    fn a_bold_lead_in_that_ends_inside_the_paragraph_keeps_its_regular_lines() {
        // The first line is bold and the emphasis ends partway through the second line.
        let bold = || More {
            bold: true,
            ..More::default()
        };
        assert_eq!(
            paragraphs(vec![
                piece(
                    "Paper plans and illustrated guides for the upper shelves and units",
                    100.0,
                    500.0,
                    100.0,
                    bold()
                ),
                piece(
                    "need to be neatly marked and stored by colour.",
                    100.0,
                    330.0,
                    112.0,
                    bold()
                ),
                p("Paper-XL display stands can fold", 333.0, 500.0, 112.0),
                p(
                    "from fully open to a single flat sheet. We can use them to",
                    100.0,
                    500.0,
                    124.0
                ),
                p("show tiny paper models.", 100.0, 300.0, 136.0),
            ]),
            [
                "Paper plans and illustrated guides for the upper shelves and units need to be neatly marked and stored by colour. Paper-XL display stands can fold from fully open to a single flat sheet. We can use them to show tiny paper models."
            ]
        );
        // A heading of two bold lines stays apart from the regular text below.
        assert_eq!(
            paragraphs(vec![
                piece(
                    "Materials for moving the wooden racks, ribbons and",
                    100.0,
                    500.0,
                    100.0,
                    bold()
                ),
                piece("rows of toy boats", 100.0, 250.0, 112.0, bold()),
                p(
                    "The text below the heading goes on to the",
                    100.0,
                    500.0,
                    124.0
                ),
                p("next line.", 100.0, 200.0, 136.0),
            ])
            .len(),
            2
        );
    }

    #[test]
    fn a_bold_line_inside_a_regular_sentence_goes_on_with_it() {
        let bold = || More {
            bold: true,
            ..More::default()
        };
        assert_eq!(
            paragraphs(vec![
                p(
                    "Decorative lanterns are bright and colorful, lightweight and fold-",
                    100.0,
                    500.0,
                    100.0
                ),
                piece(
                    "able, adjustable and compact enough, and useful with",
                    100.0,
                    500.0,
                    112.0,
                    bold()
                ),
                p("glass marbles inside.", 100.0, 300.0, 124.0),
            ]),
            [
                "Decorative lanterns are bright and colorful, lightweight and foldable, adjustable and compact enough, and useful with glass marbles inside."
            ]
        );
        // After a finished line, a bold line is a heading of its own.
        assert_eq!(
            paragraphs(vec![
                p(
                    "The paragraph before the heading ends on a full line.",
                    100.0,
                    500.0,
                    100.0
                ),
                piece(
                    "A bold heading as wide as the text column above it",
                    100.0,
                    500.0,
                    112.0,
                    bold()
                ),
            ])
            .len(),
            2
        );
    }

    #[test]
    fn a_ragged_line_ends_short_when_its_next_word_was_too_long_to_fit() {
        // The third line leaves 64 pt; `extraordinaria` needs about 70 pt.
        assert_eq!(
            paragraphs(vec![
                p(
                    "Además, la caja se apoya suavemente en la esquina del balcón",
                    90.0,
                    517.0,
                    100.0
                ),
                p(
                    "púrpura, metálico o rectangular del cuarto o gabinete de la casa",
                    90.0,
                    492.0,
                    112.0
                ),
                p(
                    "mientras guardas una linterna, tanto si es de fieltro o cartón",
                    90.0,
                    453.0,
                    124.0
                ),
                p(
                    "extraordinaria plegable, con un borde dorado o de color verde",
                    90.0,
                    514.0,
                    136.0
                ),
                p("y una diminuta tira metálica.", 90.0, 250.0, 148.0),
                p(
                    "Un párrafo nuevo empieza aquí y sigue hasta el borde derecho",
                    90.0,
                    512.0,
                    160.0
                ),
            ]),
            [
                "Además, la caja se apoya suavemente en la esquina del balcón púrpura, metálico o rectangular del cuarto o gabinete de la casa mientras guardas una linterna, tanto si es de fieltro o cartón extraordinaria plegable, con un borde dorado o de color verde y una diminuta tira metálica.",
                "Un párrafo nuevo empieza aquí y sigue hasta el borde derecho"
            ]
        );
    }

    #[test]
    fn line_joints_are_shared_by_both_extractions() {
        assert_eq!(joint("compli-", "cated"), Joint::Dehyphenate);
        assert_eq!(
            join_lines("compli-", "cated sentence"),
            "complicated sentence"
        );
        assert_eq!(join_lines("well-", "Known"), "well- Known");
        assert_eq!(
            join_lines("模型展示団", "体や案内係"),
            "模型展示団体や案内係"
        );
        assert_eq!(join_lines("one", "two"), "one two");
        assert!(ends_open("…予定の丸い台座") && ends_open("they are ini-"));
        assert!(!ends_open("考えられる。") && !ends_open("(see above).”"));
        assert!(starts_continuation("はこの棚の右奥の箱") && starts_continuation("the strips"));
        assert!(starts_continuation("、そして") && !starts_continuation("なお、"));
        assert!(!starts_continuation("The next") && !starts_continuation("第１に"));
        assert!(starts_item("• item") && starts_item("2. Engin") && starts_item("①"));
        assert!(starts_item("（２）「模型の並べ方」") && starts_item("１．ペーパーモデルの展示"));
        assert!(!starts_item("１０月には") && !starts_item("（注）"));
        assert!(!starts_item("2018) used") && !starts_item("trans-"));
    }

    #[test]
    fn thresholds_are_in_double_precision() {
        let size = |s| More {
            size: Some(s),
            ..More::default()
        };
        // A gap of 121.8 − 120 against 0.15 × 12: not wider in doubles (wider in f32).
        assert_eq!(
            paragraphs(vec![
                piece("a", 100.0, 120.0, 100.0, size(12.0)),
                piece("b", 121.8, 140.0, 100.0, size(12.0)),
            ]),
            ["ab"]
        );
    }
}
