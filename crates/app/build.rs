//! Platform-specific linking and resources.
//!
//! - Windows: embeds the application icon and version information. GPUI loads icon resource
//!   ID 1 for its windows, which is the ID `set_icon` uses.
//! - Linux: sets the runtime library path to the executable's own directory (`$ORIGIN`), where
//!   packages ship the llama.cpp shared libraries. This is the executable's location, never the
//!   current directory, and Windows looks next to the exe for DLLs the same way.

fn main() {
    println!("cargo:rerun-if-changed=../../assets/icon/icon.ico");
    println!("cargo:rerun-if-changed=locales");
    println!("cargo:rustc-check-cfg=cfg(private_translate_bundled_metal)");
    println!("cargo:rerun-if-env-changed=GGML_PRIVATE_TRANSLATE_METALLIB_DIR");
    match std::env::var("CARGO_CFG_TARGET_OS").as_deref() {
        Ok("linux") => {
            println!("cargo:rustc-link-arg-bins=-Wl,-rpath,$ORIGIN");
            return;
        }
        Ok("macos") => {
            if std::env::var_os("GGML_PRIVATE_TRANSLATE_METALLIB_DIR").is_some() {
                println!("cargo:rustc-cfg=private_translate_bundled_metal");
            }
            return;
        }
        Ok("windows") => {}
        _ => return,
    }
    let mut res = winresource::WindowsResource::new();
    res.set_icon("../../assets/icon/icon.ico")
        .set("ProductName", "Private Translate")
        .set("FileDescription", "Private Translate")
        .set(
            "LegalCopyright",
            "MIT / Apache License 2.0. See the bundled license files.",
        );
    if let Err(e) = res.compile() {
        panic!("failed to embed Windows resources: {e}");
    }
}
