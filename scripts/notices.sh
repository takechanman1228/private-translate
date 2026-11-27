#!/bin/sh
# Writes THIRD-PARTY-NOTICES.txt for a package: the licenses of the Rust crates (cargo-about,
# configured by about.toml and about.hbs) followed by the components cargo-about cannot see.
#
# usage: scripts/notices.sh PLATFORM OUTPUT
set -eu
platform=$1
output=$2
case "$platform" in
    macos) ;;
    *) echo "Unsupported notices platform: $platform" >&2; exit 2 ;;
esac
cd "$(dirname "$0")/.."

# about.toml lists the Windows and Linux targets; the macOS package has its own crate set.
set --
if [ "$platform" = macos ]; then
    set -- --target aarch64-apple-darwin
fi
{
    ./scripts/local-dev.sh about generate --locked "$@" about.hbs
    echo "Other components"
    echo "================"
    echo
    if [ "${PRIVATE_TRANSLATE_STATIC_LLAMA:-0}" = 1 ]; then
        echo 'llama.cpp and ggml are statically linked in this CPU build.'
        sed -n '/^MIT License/,$p' licenses/llama.cpp.txt
    else
        cat licenses/llama.cpp.txt
    fi
    echo
    if [ "$platform" = macos ]; then
        echo "llama.cpp is MIT-licensed. The macOS package patch loads bundled precompiled Metal shaders, with embedded shaders as a fallback."
        echo
    fi
    cat licenses/lucide.txt
    if [ -n "${PRIVATE_TRANSLATE_LOCAL_PDFIUM_DIR:-}" ]; then
        echo
        echo "PDFium and components"
        for notice in "$PRIVATE_TRANSLATE_LOCAL_PDFIUM_DIR"/LICENSE "$PRIVATE_TRANSLATE_LOCAL_PDFIUM_DIR"/licenses/*; do
            if [ ! -f "$notice" ] && [ "$notice" = "$PRIVATE_TRANSLATE_LOCAL_PDFIUM_DIR/LICENSE" ]; then
                continue
            fi
            echo
            echo "$(basename "$notice")"
            cat "$notice"
        done
    fi
    if [ -n "${PRIVATE_TRANSLATE_LOCAL_IPAFONT_DIR:-}" ]; then
        echo
        echo "IPA Gothic and IPA P Gothic (unmodified IPAfont00303)"
        cat "$PRIVATE_TRANSLATE_LOCAL_IPAFONT_DIR/IPA_Font_License_Agreement_v1.0.txt"
    fi
    if [ -n "${PRIVATE_TRANSLATE_PDF_NOTO_DIR:-}" ]; then
        for family in NotoSans NotoSansSC NotoSansTC NotoSansKR; do
            echo
            echo "$family (SIL Open Font License 1.1)"
            cat "$PRIVATE_TRANSLATE_PDF_NOTO_DIR/$family-OFL.txt"
        done
    fi
} > "$output"
