#!/bin/sh
# Copy the PDF runtime, fonts, and their original license files into a macOS app.
set -eu
resources=$1
if [ -n "${PRIVATE_TRANSLATE_LOCAL_PDFIUM_DIR:-}" ]; then
    mkdir -p "$resources/pdfium-licenses"
    cp "$PRIVATE_TRANSLATE_LOCAL_PDFIUM_DIR/lib/libpdfium.dylib" "$resources/libpdfium.dylib"
    cp "$PRIVATE_TRANSLATE_LOCAL_PDFIUM_DIR"/licenses/* "$resources/pdfium-licenses/"
    cp "$PRIVATE_TRANSLATE_LOCAL_PDFIUM_DIR/LICENSE" "$resources/pdfium-licenses/LICENSE"
fi
if [ -n "${PRIVATE_TRANSLATE_LOCAL_IPAFONT_DIR:-}" ]; then
    mkdir -p "$resources/ipafont"
    cp "$PRIVATE_TRANSLATE_LOCAL_IPAFONT_DIR/ipag.ttf" "$resources/ipafont/"
    if [ -f "$PRIVATE_TRANSLATE_LOCAL_IPAFONT_DIR/ipagp.ttf" ]; then
        cp "$PRIVATE_TRANSLATE_LOCAL_IPAFONT_DIR/ipagp.ttf" "$resources/ipafont/"
    fi
    cp "$PRIVATE_TRANSLATE_LOCAL_IPAFONT_DIR/IPA_Font_License_Agreement_v1.0.txt" "$resources/ipafont/"
    cp "$PRIVATE_TRANSLATE_LOCAL_IPAFONT_DIR/Readme_IPAfont00303.txt" "$resources/ipafont/"
fi
python3 "$(dirname "$0")/bundle-noto.py" "$resources/notofont"
PRIVATE_TRANSLATE_PDF_NOTO_DIR="$resources/notofont" "$(dirname "$0")/notices.sh" macos "$resources/THIRD-PARTY-NOTICES.txt"
