#!/bin/sh
# Reproducible local build environment; does not edit shell profiles or publish anything.
set -eu
cd "$(dirname "$0")/.."
if [ -f "$HOME/.cargo/env" ]; then . "$HOME/.cargo/env"; fi
if [ "$(uname -s)" = Darwin ]; then
    export SDKROOT="$(xcrun --show-sdk-path)"
    # This CLT installation keeps libc++ headers in the SDK, outside clang's default search.
    if [ -f "$SDKROOT/usr/include/c++/v1/cstdlib" ]; then
        export CXXFLAGS="${CXXFLAGS:-} -isystem $SDKROOT/usr/include/c++/v1"
        export BINDGEN_EXTRA_CLANG_ARGS="${BINDGEN_EXTRA_CLANG_ARGS:-} -isystem $SDKROOT/usr/include/c++/v1"
    fi
fi
if [ "${1:-}" = package-macos ]; then
    shift
    exec ./scripts/package-macos.sh "$@"
fi
exec cargo "$@"
