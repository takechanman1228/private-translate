//! Bundled static TrueType faces, in target-language preference order.
use super::{Error, resource_dirs};
use std::path::PathBuf;
use ttf_parser::{Face, GlyphId};

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum Family {
    #[default]
    Ipa,
    Latin,
    Sc,
    Tc,
    Kr,
}

impl Family {
    pub fn for_language(language: &str) -> Self {
        match language {
            "ja" | "ja(easy)" | "en" => Self::Ipa,
            "zh" | "zh-hans" => Self::Sc,
            "zh-hant" => Self::Tc,
            "ko" => Self::Kr,
            _ => Self::Latin,
        }
    }
}

// Languages offered by the app whose writing systems need shaping, bidi,
// or glyphs outside this bundle (Armenian and Georgian).
pub fn supports_pdf(language: &str) -> bool {
    !matches!(
        language,
        "ar" | "bn"
            | "fa"
            | "gu"
            | "he"
            | "hi"
            | "hy"
            | "ka"
            | "km"
            | "ml"
            | "mr"
            | "my"
            | "ne"
            | "pa"
            | "si"
            | "ta"
            | "te"
            | "th"
            | "ur"
    )
}

#[derive(Default)]
pub(super) struct Noto(Vec<(Family, Vec<u8>)>);

impl Noto {
    pub fn load() -> Self {
        let dirs = std::env::var_os("PRIVATE_TRANSLATE_PDF_NOTO_DIR").map_or_else(
            || {
                resource_dirs(
                    &std::env::current_exe().unwrap_or_default(),
                    cfg!(target_os = "macos"),
                )
                .into_iter()
                .map(|p| p.join("notofont"))
                .collect()
            },
            |p| vec![PathBuf::from(p)],
        );
        let mut fonts = Vec::new();
        for (family, suffix) in [
            (Family::Latin, ""),
            (Family::Sc, "SC"),
            (Family::Tc, "TC"),
            (Family::Kr, "KR"),
        ] {
            let name = format!("NotoSans{suffix}-Regular.ttf");
            if let Some(bytes) = dirs
                .iter()
                .find_map(|dir| std::fs::read(dir.join(&name)).ok())
                && Face::parse(&bytes, 0)
                    .is_ok_and(|f| f.tables().glyf.is_some() && !f.is_variable())
            {
                fonts.push((family, bytes));
            }
        }
        Self(fonts)
    }
}

pub(super) struct Faces<'a> {
    pub fonts: Vec<Face<'a>>,
    pub ipa_first: bool,
}

impl<'a> Faces<'a> {
    pub fn new(ipa: &'a [u8], noto: &'a Noto, primary: Family) -> Result<Self, Error> {
        let mut entries = vec![(Family::Ipa, Face::parse(ipa, 0).map_err(|_| Error::Font)?)];
        for (family, bytes) in &noto.0 {
            entries.push((*family, Face::parse(bytes, 0).map_err(|_| Error::Font)?));
        }
        entries.sort_by_key(|(family, _)| *family != primary);
        Ok(Self {
            ipa_first: entries[0].0 == Family::Ipa,
            fonts: entries.into_iter().map(|(_, face)| face).collect(),
        })
    }

    pub fn exact(&self, ch: char) -> Option<(usize, GlyphId)> {
        self.fonts
            .iter()
            .enumerate()
            .find_map(|(i, f)| f.glyph_index(ch).map(|g| (i, g)))
    }
    pub fn glyph(&self, ch: char, added: bool) -> Option<(usize, GlyphId)> {
        self.exact(ch).or_else(|| {
            self.fonts.iter().enumerate().find_map(|(i, f)| {
                super::inplace::translation_glyph(f, ch)
                    .or_else(|| (added && ch == 'µ').then(|| f.glyph_index('μ')).flatten())
                    .map(|g| (i, g))
            })
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn targets_select_regional_faces_and_text_export() {
        for (code, family) in [
            ("ja", Family::Ipa),
            ("en", Family::Ipa),
            ("zh-hans", Family::Sc),
            ("zh-hant", Family::Tc),
            ("ko", Family::Kr),
            ("fr", Family::Latin),
            ("ru", Family::Latin),
            ("el", Family::Latin),
        ] {
            assert_eq!(Family::for_language(code), family);
            assert!(supports_pdf(code));
        }
        for code in ["ar", "he", "th", "hi", "bn", "km", "my", "ur"] {
            assert!(!supports_pdf(code));
        }
    }

    #[test]
    #[ignore = "requires IPA and the bundled Noto fonts"]
    fn fallback_uses_regional_outlines_and_tracks_missing_assets() {
        let ipa = super::super::output_font().unwrap();
        let noto = Noto::load();
        assert_eq!(noto.0.len(), 4);
        for family in [Family::Sc, Family::Tc, Family::Kr] {
            let faces = Faces::new(&ipa, &noto, family).unwrap();
            assert_eq!(faces.exact('骨').unwrap().0, 0);
            assert_eq!(
                faces.fonts[0].raw_face().data,
                noto.0.iter().find(|(f, _)| *f == family).unwrap().1
            );
        }
        let faces = Faces::new(&ipa, &noto, Family::Latin).unwrap();
        assert_eq!(faces.exact('é').unwrap().0, 0);
        assert_eq!(faces.exact('日').unwrap().0, 1);
        assert!(faces.exact('한').unwrap().0 > 1);
        let empty = Noto::default();
        let faces = Faces::new(&ipa, &empty, Family::Kr).unwrap();
        assert!(faces.ipa_first);
        assert!(faces.exact('한').is_none());
    }
}
