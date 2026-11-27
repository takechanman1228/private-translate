# Bundled Metal libraries

llama.cpp is MIT-licensed. The macOS package patch loads bundled precompiled Metal shaders, with embedded shaders as a fallback.

`bundled-metal.patch` patches `ggml-metal-device.m` from llama.cpp
`8642541b255bfb5dc39cd351a178be09013a55d6`, vendored by the pinned
llama-cpp-rs dependency in `Cargo.toml`. The macOS packager applies it to a
build-directory copy through `package-metal.cmake`. Ordinary Cargo builds use
the upstream checkout and embedded MSL.

The patched backend first tries the main bundle's `default.metallib` and, when
the existing device checks enable the Tensor API, `ggml-tensor.metallib`. If a
required library is missing or unreadable, or BF16 is enabled, it compiles the
embedded sources with the original device features. The precompiled libraries
match the app's default BF16-disabled configuration. The app reports the selected path in its own log when loading a model on Metal.
The package smoke test reports the path for the detected Metal device.

The base library uses Metal 3.0 (available from macOS 13.0) targeting macOS 13.3
by default. The Tensor library uses Metal 4.0 targeting macOS 26.0. Keep the
Tensor sources in the CMake hook in sync with upstream when updating the pin.

Set `PRIVATE_TRANSLATE_METAL_DEVELOPER_DIR` to the Metal-capable Xcode’s
`Contents/Developer` when the active developer tools lack Metal. The Rust and
CMake build continues to use the active macOS SDK.
