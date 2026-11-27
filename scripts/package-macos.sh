#!/bin/bash
# Builds the macOS (Apple Silicon) package: an app bundle in a disk image.
#
# usage: scripts/package-macos.sh [build|sign|sign-test|smoke]
# With no argument, build, sign ad hoc, and smoke test in one run. For Developer ID
# signing, run build without signing credentials, then sign the resulting dist/ folder.
# Run the release smoke test on a separate runner with no signing credentials.
# sign-test is for a self-signed identity in a temporary keychain. It keeps Hardened
# Runtime on and relaxes library validation because test certificates have no Team ID.
#
# Compiling the shaders needs Xcode 26 or later with the Metal Toolchain. The bundle includes
# precompiled Metal libraries and retains llama.cpp's embedded shaders as a fallback.
# The default signature is ad hoc. Gatekeeper blocks quarantined ad-hoc bundles.
# When CODESIGN_IDENTITY names a signing identity, the bundle is
# signed with the hardened runtime and a secure timestamp, as notarization requires.
#
# Environment:
#   MACOSX_DEPLOYMENT_TARGET  oldest macOS to run on (default 13.3, as llama.cpp's Apple builds)
#   CODESIGN_IDENTITY         Developer ID Application identity (default "-", ad hoc)
#   CODESIGN_KEYCHAIN         optional keychain containing the identity
#   PACKAGE_CODESIGN_TIMESTAMP  "none" for local test identities (default: secure timestamp)
#   PACKAGE_VERIFY_NOTARIZATION  "1" to validate both staples and assess the mounted app in smoke
#   NOTARY_KEY_ID             App Store Connect team API key ID
#   NOTARY_ISSUER_ID          App Store Connect team API issuer ID
#   NOTARY_KEY_FILE           path to the private .p8 key (set all three or none)
#   PRIVATE_TRANSLATE_METAL_DEVELOPER_DIR  selects Xcode for metal and metallib only
#   CARGO_TARGET_DIR          optional Cargo build cache root (default: target/macos-package)
#   PACKAGE_VERSION           the version in the package's name (default: from the tags, below)
#   PRIVATE_TRANSLATE_LOCAL_PDFIUM_DIR     extracted macOS arm64 PDFium distribution
#   PRIVATE_TRANSLATE_LOCAL_IPAFONT_DIR    extracted, unmodified IPAfont00303 distribution
#   PRIVATE_TRANSLATE_PDF_NOTO_DIR        optional cache of the pinned Noto fonts and licenses
#
# Output: dist/<package>/ and <package>.dmg in the repository root.
set -eu
cd "$(dirname "$0")/.."

test "$(uname -s)" = Darwin || { echo "this script runs on macOS" >&2; exit 1; }

test "$(uname -m)" = arm64 || { echo "this package requires Apple Silicon" >&2; exit 1; }
mode=${1:-all}
case "$mode" in
    all|build|sign|sign-test|smoke) ;;
    *) echo "Usage: $0 [build|sign|sign-test|smoke]" >&2; exit 2 ;;
esac

export MACOSX_DEPLOYMENT_TARGET="${MACOSX_DEPLOYMENT_TARGET:-13.3}"
identity="${CODESIGN_IDENTITY:--}"
timestamp="${PACKAGE_CODESIGN_TIMESTAMP:-secure}"
if [ "$mode" = all ] && [ "$identity" != - ]; then
    echo "Run build without signing credentials, then sign and smoke separately." >&2
    exit 2
fi

work=$(mktemp -d)
mount="$work/mount"
mounted=false
cleanup() {
    if "$mounted"; then
        hdiutil detach -quiet "$mount" || return 1
    fi
    rm -rf "$work"
}
trap cleanup EXIT
trap 'exit 1' HUP INT TERM

# Get the account home separately so the leak check still works with an overridden HOME.
builder_home=$(python3 -c 'import os, pwd; print(pwd.getpwuid(os.getuid()).pw_dir)')
test -n "$builder_home" && test "$builder_home" != /
workspace=$(pwd -P)
package_target_root="${CARGO_TARGET_DIR:-target/macos-package}"
metal_patch_dir="$workspace/patches/llama.cpp"

metal_xcrun() {
    DEVELOPER_DIR="${PRIVATE_TRANSLATE_METAL_DEVELOPER_DIR:-${DEVELOPER_DIR:-}}" /usr/bin/xcrun -sdk macosx "$@"
}

metal_build_cache() {
    local sdk_version metal_sdk metal_version linker_version app_sdk app_clang build_key
    if ! sdk_version=$(metal_xcrun --show-sdk-version 2>/dev/null) ||
        ! [ "${sdk_version%%.*}" -ge 26 ] 2>/dev/null ||
        ! metal_sdk=$(metal_xcrun --show-sdk-path 2>/dev/null) ||
        ! metal_version=$(metal_xcrun metal --version 2>/dev/null) ||
        ! linker_version=$(metal_xcrun metallib --version 2>/dev/null); then
        echo "Packaging requires Xcode 26 or later with the Metal Toolchain (metal and metallib)." >&2
        echo "Set PRIVATE_TRANSLATE_METAL_DEVELOPER_DIR to that Xcode's Contents/Developer." >&2
        echo "To install the component, run xcodebuild -downloadComponent MetalToolchain with DEVELOPER_DIR set to that Xcode." >&2
        exit 1
    fi

    app_sdk=$(xcrun --sdk macosx --show-sdk-path)
    app_clang=$(xcrun --sdk macosx clang --version)

    # llama-cpp-sys-2 reuses its CMake configuration inside a Cargo target dir.
    # Keep packaged builds separate from ordinary Cargo builds and invalidate
    # that configuration when its inputs change.
    build_key=$(
        {
            printf '%s\n' "$MACOSX_DEPLOYMENT_TARGET" "$app_sdk" "$app_clang" \
                "${PRIVATE_TRANSLATE_METAL_DEVELOPER_DIR:-}" "$metal_sdk" "$sdk_version" "$metal_version" "$linker_version"
            shasum -a 256 Cargo.lock "$metal_patch_dir/package-metal.cmake" "$metal_patch_dir/bundled-metal.patch"
        } | shasum -a 256 | cut -c 1-16
    )
    mkdir -p "$package_target_root/metal-$build_key"
    package_target_dir=$(cd "$package_target_root/metal-$build_key" && pwd -P)
}

require_metal_libraries() {
    local bundle=$1 lib
    for lib in default.metallib ggml-tensor.metallib; do
        if [[ ! -f "$bundle/Contents/Resources/$lib" || ! -s "$bundle/Contents/Resources/$lib" ||
            -L "$bundle/Contents/Resources/$lib" ]]; then
            echo "Missing bundled Metal library: $lib" >&2
            exit 1
        fi
    done
}

build_release() (
    cd "$workspace"
    # Keep arguments containing spaces intact for rustc and cc-rs. Remap the C/C++/Obj-C
    # paths too: ggml and embedded Metal shaders use file names in diagnostics.
    separator=$(printf '\037')
    rustflags="${CARGO_ENCODED_RUSTFLAGS:-}"
    native_flags=""
    # rustc uses the last matching prefix. Put the more specific paths last.
    for mapping in "$builder_home=/build/home" "$HOME=/build/home" \
        "${CARGO_HOME:-$HOME/.cargo}=/src/cargo" "${RUSTUP_HOME:-$HOME/.rustup}=/src/rustup" \
        "$workspace=/src/private-translate" "$package_target_dir=/build/target"; do
        rustflags="${rustflags:+$rustflags$separator}--remap-path-prefix=$mapping"
        native_flags="$native_flags \"-ffile-prefix-map=$mapping\""
    done
    export CARGO_ENCODED_RUSTFLAGS="$rustflags" CARGO_INCREMENTAL=0 CC_SHELL_ESCAPED_FLAGS=1
    export CFLAGS="${CFLAGS:-}$native_flags" CXXFLAGS="${CXXFLAGS:-}$native_flags"
    # CMake's Objective-C compiler uses OBJCFLAGS rather than CFLAGS.
    export OBJCFLAGS="${OBJCFLAGS:-}$native_flags"
    export CMAKE_PROJECT_INCLUDE="$metal_patch_dir/package-metal.cmake"
    export GGML_METAL_EMBED_LIBRARY=ON GGML_PRIVATE_TRANSLATE_METALLIB_DIR="$package_target_dir/metallibs"
    ./scripts/local-dev.sh build \
        -p private-translate --profile dist --locked --target-dir "$package_target_dir"
)

# The version, as the Release workflow names it (which passes it): X.Y.Z on a matching tag, else
# X.Y.Z-dev.<commit>.
crate=$(sed -n 's/^version = "\(.*\)"/\1/p' Cargo.toml | head -n1)
if [ -n "${PACKAGE_VERSION:-}" ]; then
    version=$PACKAGE_VERSION
elif git describe --exact-match --tags --match "v$crate" HEAD >/dev/null 2>&1; then
    version=$crate
else
    version="$crate-dev.$(git rev-parse --short HEAD)"
fi
package="private-translate-$version-macos-arm64"
app_name="Private Translate"
case "$version" in
    ''|*[!a-zA-Z0-9.-]*) echo "Invalid package version." >&2; exit 2 ;;
esac
dist="dist/$package"
app="$dist/$app_name.app"
binary="$app/Contents/MacOS/private-translate"
image="$package.dmg"

build_bundle() (
    unset CODESIGN_IDENTITY CODESIGN_KEYCHAIN NOTARY_KEY_ID NOTARY_ISSUER_ID NOTARY_KEY_FILE
    metal_build_cache
    : "${PRIVATE_TRANSLATE_LOCAL_PDFIUM_DIR:?Set to the extracted macOS arm64 PDFium distribution}"
    : "${PRIVATE_TRANSLATE_LOCAL_IPAFONT_DIR:?Set to the extracted IPAfont00303 directory}"
    lipo "$PRIVATE_TRANSLATE_LOCAL_PDFIUM_DIR/lib/libpdfium.dylib" -verify_arch arm64
    echo "Building $package (macOS $MACOSX_DEPLOYMENT_TARGET or later)"
    build_release
    rm -rf "$dist" "$image"
    mkdir -p "$app/Contents/MacOS" "$app/Contents/Resources"

    cp "$package_target_dir/dist/private-translate" "$binary"
    for lib in default.metallib ggml-tensor.metallib; do
        cp "$package_target_dir/metallibs/$lib" "$app/Contents/Resources/"
        check_paths "$app/Contents/Resources/$lib"
    done
    require_metal_libraries "$app"
    xcrun vtool -show-build "$binary"
    ./scripts/bundle-macos-pdf.sh "$app/Contents/Resources"
    cp LICENSE-MIT LICENSE-APACHE COPYRIGHT "$app/Contents/Resources/"
    if [ -f NOTICE ]; then
        cp NOTICE "$app/Contents/Resources/"
    fi

    # The icon, rendered from the same design as the other platforms' (scripts/gen_icons.py), with
    # the margin macOS icons have: the tile covers 824 of 1024 pixels.
    iconset="$work/AppIcon.iconset"
    mkdir -p "$iconset"
    python3 -B - "$iconset/icon_512x512@2x.png" <<'EOF'
import sys
sys.path.insert(0, "scripts")
import gen_icons

canvas, tile = 1024, 824
margin = (canvas - tile) // 2
rows = gen_icons.render(tile, ss=2)
empty = (0, 0, 0, 0)
padded = [[empty] * canvas for _ in range(margin)]
padded += [[empty] * margin + row + [empty] * margin for row in rows]
padded += [[empty] * canvas for _ in range(canvas - len(padded))]
with open(sys.argv[1], "wb") as f:
    f.write(gen_icons.png_bytes(padded))
EOF
    for size in 16 32 128 256 512; do
        sips -z "$size" "$size" "$iconset/icon_512x512@2x.png" --out "$iconset/icon_${size}x${size}.png" >/dev/null
        double=$((size * 2))
        if [ "$size" != 512 ]; then
            sips -z "$double" "$double" "$iconset/icon_512x512@2x.png" --out "$iconset/icon_${size}x${size}@2x.png" >/dev/null
        fi
    done
    iconutil -c icns "$iconset" -o "$app/Contents/Resources/AppIcon.icns"

    cat > "$app/Contents/Info.plist" <<EOF
<?xml version="1.0" encoding="UTF-8"?>
<!DOCTYPE plist PUBLIC "-//Apple//DTD PLIST 1.0//EN" "http://www.apple.com/DTDs/PropertyList-1.0.dtd">
<plist version="1.0">
<dict>
    <key>CFBundleDevelopmentRegion</key>
    <string>en</string>
    <key>CFBundleDisplayName</key>
    <string>$app_name</string>
    <key>CFBundleExecutable</key>
    <string>private-translate</string>
    <key>CFBundleIconFile</key>
    <string>AppIcon</string>
    <key>CFBundleIdentifier</key>
    <string>io.github.takechanman1228.private-translate</string>
    <key>CFBundleInfoDictionaryVersion</key>
    <string>6.0</string>
    <key>CFBundleName</key>
    <string>Private Translate</string>
    <key>CFBundlePackageType</key>
    <string>APPL</string>
    <key>CFBundleShortVersionString</key>
    <string>$crate</string>
    <key>CFBundleVersion</key>
    <string>$crate</string>
    <key>LSApplicationCategoryType</key>
    <string>public.app-category.productivity</string>
    <key>LSMinimumSystemVersion</key>
    <string>$MACOSX_DEPLOYMENT_TARGET</string>
    <key>NSHighResolutionCapable</key>
    <true/>
    <key>NSHumanReadableCopyright</key>
    <string>MIT / Apache License 2.0. See the bundled license files.</string>
    <key>NSPrincipalClass</key>
    <string>NSApplication</string>
</dict>
</plist>
EOF
    plutil -lint "$app/Contents/Info.plist"

    # What the app needs to update itself; inside the bundle, so before it is signed.
    python3 scripts/package-marker.py "$app/Contents/Resources" macos-arm64 --no-entries

    check_paths "$binary"
    cp README.md README.ja.md LICENSE-MIT LICENSE-APACHE COPYRIGHT "$dist/"
    cp "$app/Contents/Resources/THIRD-PARTY-NOTICES.txt" "$dist/"
    ln -s /Applications "$dist/Applications"
    ls -l "$dist" "$app/Contents/MacOS"
)

check_paths() {
    local checked_file=$1 prefix count
    # Search the packaged file without printing any matched path.
    for prefix in "$builder_home" "$HOME" "$workspace" "${package_target_dir:-}"; do
        [ -n "$prefix" ] && [ "$prefix" != / ] || continue
        if LC_ALL=C grep -aqF -- "$prefix" "$checked_file"; then
            count=$(strings -a "$checked_file" | grep -cF -- "$prefix") || true
            echo "The packaged ${checked_file##*/} contains a build path ($count matching strings)." >&2
            exit 1
        else
            test "$?" -eq 1
        fi
    done
    echo "Packaged ${checked_file##*/} path check passed (0 matches)."
}

# Sign Mach-O components and nested code bundles in depth-first order. The main
# executable is signed by signing the outer bundle. PDFium shares the app's
# identity, which lets library validation stay on. llama.cpp is static and
# compiles Metal shaders through Metal, so we pass no executable-memory entitlements.
sign() {
    if [ -n "${CODESIGN_KEYCHAIN:-}" ]; then
        set -- --keychain "$CODESIGN_KEYCHAIN" "$@"
    fi
    if [ "$identity" = - ]; then
        codesign --force --sign - --timestamp=none "$@"
    elif [ "$timestamp" = none ]; then
        codesign --force --sign "$identity" --timestamp=none "$@"
    else
        codesign --force --sign "$identity" --timestamp "$@"
    fi
}

sign_code() {
    if [ "$identity" != - ]; then
        set -- --options runtime "$@"
    fi
    sign "$@"
}

require_no_entitlements() {
    local component=$1 entitlements
    shift
    if ! entitlements=$(codesign -d --entitlements - --xml "$@" "$component" 2>/dev/null); then
        echo "Could not inspect signed entitlements in $component." >&2
        exit 1
    fi
    # codesign emits no XML when entitlements are absent, or a plist dictionary.
    if [[ "$entitlements" == *'<key'* ]]; then
        echo "Unexpected signed entitlements in $component." >&2
        exit 1
    fi
}

check_bundle_entitlements() {
    local bundle=$1 component kind architectures arch
    local -a slices
    require_no_entitlements "$bundle"
    find "$bundle" -type f -print0 > "$work/entitlement-components"
    while IFS= read -r -d '' component; do
        kind=$(file -b "$component")
        case "$kind" in
            Mach-O*)
                architectures=$(lipo -archs "$component")
                read -r -a slices <<< "$architectures"
                for arch in "${slices[@]}"; do
                    require_no_entitlements "$component" --arch "$arch"
                done
                ;;
        esac
    done < "$work/entitlement-components"
}

notarize_and_staple() {
    echo "Submitting $1 for notarization."
    if ! xcrun notarytool submit "$1" --wait --timeout 30m --output-format plist \
        --key-id "$NOTARY_KEY_ID" --issuer "$NOTARY_ISSUER_ID" --key "$NOTARY_KEY_FILE" \
        > "$work/notary.plist" 2> "$work/notary.stderr"; then
        echo "notarytool submit failed. Check the API key and the Apple notarization submission history." >&2
        exit 1
    fi
    if [ "$(plutil -extract status raw -o - "$work/notary.plist")" != Accepted ]; then
        echo "Apple did not accept the submission. Inspect the notarization log in the submission history." >&2
        exit 1
    fi
    xcrun stapler staple "$2"
    xcrun stapler validate "$2"
}

sign_and_package() {
    for directory in "$dist" "$app" "$app/Contents" "$app/Contents/MacOS"; do
        if [ ! -d "$directory" ] || [ -L "$directory" ]; then
            echo "Expected a built bundle in $dist." >&2
            exit 1
        fi
    done
    if [ ! -x "$binary" ] || [ -L "$binary" ]; then
        echo "Build $app first." >&2
        exit 1
    fi
    require_metal_libraries "$app"
    if [ "$mode" = sign-test ]; then
        if [ "$identity" = - ] || [ -z "${CODESIGN_KEYCHAIN:-}" ]; then
            echo "Test signing requires an identity in an explicit temporary keychain." >&2
            exit 2
        fi
        timestamp=none
    fi
    case "$timestamp" in
        secure|none) ;;
        *) echo "Invalid PACKAGE_CODESIGN_TIMESTAMP value." >&2; exit 2 ;;
    esac
    notarize=false
    if [ -n "${NOTARY_KEY_ID:-}${NOTARY_ISSUER_ID:-}${NOTARY_KEY_FILE:-}" ]; then
        if [ "$identity" = - ] || [ "$timestamp" != secure ] || [ -z "${NOTARY_KEY_ID:-}" ] ||
            [ -z "${NOTARY_ISSUER_ID:-}" ] || [ ! -s "${NOTARY_KEY_FILE:-}" ]; then
            echo "Notarization requires timestamped signing, key ID, issuer ID and a readable key file." >&2
            exit 1
        fi
        notarize=true
    fi

    find "$app/Contents" -depth \
        \( -type f -o \( -type d \( -name '*.framework' -o -name '*.app' -o -name '*.xpc' -o -name '*.appex' \) \) \) \
        -print0 > "$work/nested"
    while IFS= read -r -d '' component; do
        [ "$component" != "$binary" ] || continue
        if [ -d "$component" ]; then
            sign_code "$component"
        else
            case "$(file -b "$component")" in
                Mach-O*) sign_code "$component" ;;
            esac
        fi
    done < "$work/nested"
    if [ "$mode" = sign-test ]; then
        cat > "$work/test-entitlements.plist" <<'EOF'
<?xml version="1.0" encoding="UTF-8"?>
<plist version="1.0">
<dict>
    <key>com.apple.security.cs.disable-library-validation</key>
    <true/>
</dict>
</plist>
EOF
        sign_code --entitlements "$work/test-entitlements.plist" "$app"
        details=$(codesign -dv "$app" 2>&1)
        if [ "$(sed -n 's/^TeamIdentifier=//p' <<< "$details")" != 'not set' ]; then
            echo "Test signing requires an app signature without a Team ID." >&2
            exit 1
        fi
    else
        sign_code "$app"
    fi
    codesign --verify --deep --strict --verbose=2 "$app"
    if [ "$mode" != sign-test ]; then
        check_bundle_entitlements "$app"
    fi

    check_paths "$binary"
    if "$notarize"; then
        # Submit a ZIP to get a ticket for the app itself, then put the stapled app in the DMG.
        /usr/bin/ditto -c -k --keepParent "$app" "$work/app.zip"
        notarize_and_staple "$work/app.zip" "$app"
    fi

    # hdiutil fails now and then on CI machines (a busy resource): it is tried again.
    attempt=1
    until hdiutil create -volname "$app_name" -srcfolder "$dist" -fs HFS+ -format UDZO -ov "$image"; do
        if [ "$attempt" -ge 3 ]; then
            echo "hdiutil failed $attempt times" >&2
            exit 1
        fi
        attempt=$((attempt + 1))
        sleep 10
    done
    if [ "$identity" != - ]; then
        sign --identifier "io.github.takechanman1228.private-translate.dmg" "$image"
        codesign --verify --strict "$image"
    fi
    if "$notarize"; then
        notarize_and_staple "$image" "$image"
    fi
}

smoke_package() {
    verify_notarization="${PACKAGE_VERIFY_NOTARIZATION:-0}"
    if [ "$verify_notarization" = 1 ]; then
        xcrun stapler validate "$image"
        spctl --assess --type open --context context:primary-signature --verbose=2 "$image"
    fi

    # Test the executable as shipped, using no settings, models, or running app instance.
    mkdir -p "$mount" "$work/home" "$work/tmp" "$work/data/models"
    hdiutil attach -nobrowse -readonly -noautoopen -owners off -stdinpass \
        -mountpoint "$mount" "$image" </dev/null
    mounted=true
    shipped="$mount/$app_name.app"
    codesign --verify --deep --strict --verbose=2 "$shipped"
    require_metal_libraries "$shipped"
    echo "Packaged Metal libraries are present."
    if [ "$verify_notarization" = 1 ]; then
        check_bundle_entitlements "$shipped"
        xcrun stapler validate "$shipped"
        spctl --assess --type execute --ignore-cache --no-cache --verbose=2 "$shipped"
    fi
    smoke_binary="$shipped/Contents/MacOS/private-translate"
    if ! LC_ALL=C grep -aqF -- 'Metal shaders: precompiled (bundled Metal libraries)' "$smoke_binary"; then
        echo "Packaged executable is missing the bundled Metal library code path." >&2
        exit 1
    fi
    check_paths "$smoke_binary"

    python3 - "$work/smoke.pdf" <<'EOF'
import sys

stream = b"BT /F1 12 Tf 36 72 Td (Private Translate packaging smoke test.) Tj ET"
objects = [
    b"<< /Type /Catalog /Pages 2 0 R >>",
    b"<< /Type /Pages /Kids [3 0 R] /Count 1 >>",
    b"<< /Type /Page /Parent 2 0 R /MediaBox [0 0 300 120] "
    b"/Resources << /Font << /F1 4 0 R >> >> /Contents 5 0 R >>",
    b"<< /Type /Font /Subtype /Type1 /BaseFont /Helvetica >>",
    f"<< /Length {len(stream)} >>\nstream\n".encode() + stream + b"\nendstream",
]
pdf = bytearray(b"%PDF-1.4\n")
offsets = []
for index, obj in enumerate(objects, 1):
    offsets.append(len(pdf))
    pdf.extend(f"{index} 0 obj\n".encode() + obj + b"\nendobj\n")
xref = len(pdf)
size = len(objects) + 1
pdf.extend(f"xref\n0 {size}\n0000000000 65535 f \n".encode())
for offset in offsets:
    pdf.extend(f"{offset:010d} 00000 n \n".encode())
pdf.extend(f"trailer\n<< /Size {size} /Root 1 0 R >>\nstartxref\n{xref}\n%%EOF\n".encode())
with open(sys.argv[1], "wb") as file:
    file.write(pdf)
EOF
    reported=$(env -i HOME="$work/home" CFFIXED_USER_HOME="$work/home" TMPDIR="$work/tmp" PRIVATE_TRANSLATE_LOCAL_PREVIEW=1 \
        PRIVATE_TRANSLATE_PREVIEW_DATA_DIR="$work/data" "$smoke_binary" --version)
    if [ "$reported" != "$app_name $crate" ]; then
        echo "Packaged executable smoke test did not report the workspace version." >&2
        exit 1
    fi
    env -i HOME="$work/home" CFFIXED_USER_HOME="$work/home" TMPDIR="$work/tmp" PRIVATE_TRANSLATE_LOCAL_PREVIEW=1 \
        PRIVATE_TRANSLATE_PREVIEW_DATA_DIR="$work/data" "$smoke_binary" --package-smoke-test "$work/smoke.pdf"
    echo "Packaged executable smoke test passed ($reported)."
    hdiutil detach -quiet "$mount"
    mounted=false
    shasum -a 256 "$image"
}

case "$mode" in
    build) build_bundle ;;
    sign|sign-test) sign_and_package ;;
    smoke) smoke_package ;;
    all)
        build_bundle
        sign_and_package
        smoke_package
        ;;
esac
