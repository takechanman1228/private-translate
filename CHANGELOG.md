# Changelog

## [0.1.2] - 2026-11-27

- Fixed a problem that could leave many paragraphs in the source language when using Index-Translate on long TXT files.
- When context or a glossary prevents Index-Translate from translating a Markdown file and no other suitable model is ready, the app now explains how to continue.

Requirements and install steps are unchanged from 0.1.1. See [Known limits](https://github.com/takechanman1228/private-translate#known-limits).

## [0.1.1] - 2026-11-26

Private Translate translates text and documents on your Mac with an AI model that runs locally.

**Requirements.** An Apple Silicon Mac with macOS 13.3 or later. The first run downloads a translation model (1.1 to 4.6 GB) over the internet. After that, translation works offline.

**Install.** Download `private-translate-0.1.1-macos-arm64.dmg` from this release, open it, and drag Private Translate into Applications. `SHA256SUMS.txt` lets you check the download. Open the app, check the target language, and click **Prepare**. To translate Word, PowerPoint, or PDF, click **Prepare** in the **Files** tab.

- First release, for Apple Silicon Macs.
- Translates pasted text, TXT, Markdown, Word (`.docx`), PowerPoint (`.pptx`), and PDF with selectable text. Scanned PDFs need OCR first.
- Models: Hy-MT2 and Index-Translate (Apache-2.0), and PLaMo 3 Translate (PLaMo Community License; commercial use requires registering with Preferred Networks in advance, or a separate contract for organisations with annual revenue above ¥1 billion).
- On a Mac with 8 GB of memory, Hy-MT2 1.8B translates Word, PowerPoint, and PDF, so documents can be translated without PLaMo. Index-Translate 2B translates Word, and Markdown when no context or glossary is set.
- The DMG includes precompiled Metal shaders, which shortens the first launch.
- The DMG includes PDFium and the IPA and Noto fonts, with their licences.
- Update checks are on by default, at most once a day, and can be turned off in Settings. Updates download and install only after you confirm.

See [Known limits](https://github.com/takechanman1228/private-translate#known-limits).
