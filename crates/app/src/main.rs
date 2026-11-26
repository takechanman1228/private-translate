//! Private Translate.
//!
//! Models are picked and downloaded in the app. A GGUF path given as the first argument or in
//! `PRIVATE_TRANSLATE_MODEL` overrides the saved selection, which is handy for development.

#![cfg_attr(all(windows, not(debug_assertions)), windows_subsystem = "windows")]

mod align;
mod assets;
mod backend;
mod jobs;
mod lang;
mod models;
mod setup;
mod shell;
mod view;

// Re-export shared settings, logging and glossary helpers.
pub(crate) use appdata::{app_log, applog, glossary, settings};

use std::path::PathBuf;

use gpui_kit::*;

rust_i18n::i18n!("locales", fallback = "en");

fn custom_model() -> Option<PathBuf> {
    std::env::args_os()
        .nth(1)
        .or_else(|| std::env::var_os("PRIVATE_TRANSLATE_MODEL"))
        .map(PathBuf::from)
}

/// What a macOS application has to set up itself: GPUI adds no menu bar (Cmd+Q is bound by
/// [`shell::init`] on every system).
#[cfg(target_os = "macos")]
mod macos {
    use gpui_kit::*;

    use crate::shell::{About, Quit};

    #[cfg(private_translate_bundled_metal)]
    pub(crate) fn metal_shader_selection() -> Option<&'static str> {
        unsafe extern "C" {
            fn private_translate_metal_shader_selection() -> *const std::ffi::c_char;
        }

        // SAFETY: the patched backend returns either null or a static string literal.
        unsafe {
            let selection = private_translate_metal_shader_selection();
            if selection.is_null() {
                return None;
            }
            std::ffi::CStr::from_ptr(selection).to_str().ok()
        }
    }

    pub fn set_menu(cx: &mut App) {
        cx.set_menus(vec![Menu {
            name: "Private Translate".into(),
            items: vec![
                MenuItem::action(rust_i18n::t!("about.title").into_owned(), About),
                MenuItem::action(rust_i18n::t!("tray.quit_app").into_owned(), Quit),
            ],
            disabled: false,
        }]);
    }
}

fn main() {
    #[cfg(target_os = "macos")]
    {
        let mut args = std::env::args_os().skip(1);
        match args.next().as_deref().and_then(std::ffi::OsStr::to_str) {
            Some("--version") => {
                println!("Private Translate {}", env!("CARGO_PKG_VERSION"));
                return;
            }
            Some("--package-smoke-test") => {
                let result = args
                    .next()
                    .ok_or_else(|| "expected a PDF path".to_owned())
                    .and_then(|path| package_smoke_test(&PathBuf::from(path)));
                if let Err(error) = result {
                    eprintln!("Package smoke test failed: {error}");
                    std::process::exit(1);
                }
                return;
            }
            _ => {}
        }
    }

    // A new version, started from the staging folder to swap itself in (see `update::apply`):
    // it does only that, without a window.
    if let Some(plan) = update::apply::requested() {
        applog::init(&settings::log_path());
        match update::apply::run(&plan) {
            Ok(()) => app_log!("update to {} installed", env!("CARGO_PKG_VERSION")),
            Err(e) => app_log!("update to {} failed: {e}", env!("CARGO_PKG_VERSION")),
        }
        return;
    }
    // SAFETY: first thing in main, before any thread starts.
    unsafe {
        appdata::devices::configure_openvino();
        engine::llama::configure_metal();
    }
    let custom_model = custom_model();
    applog::init(&settings::log_path());
    // The release build has no console on Windows: a panic is recorded in the log, then handled
    // as before (the message on stderr, and the thread unwinding).
    let default_hook = std::panic::take_hook();
    std::panic::set_hook(Box::new(move |info| {
        app_log!("panic: {info}");
        default_hook(info);
    }));
    settings::migrate_legacy_settings();
    let mut settings = settings::Settings::load();
    if settings::local_preview() {
        settings.check_updates = false;
        settings.double_copy = false;
        settings.resident = false;
    }
    app_log!(
        "starting {} {}; models in {}",
        env!("CARGO_PKG_NAME"),
        env!("CARGO_PKG_VERSION"),
        settings.models_dir().display()
    );
    // Files found on Hugging Face by earlier runs; the Models dialog refreshes this list.
    let cached = engine::discovery::load_cache(&settings::catalog_cache_path());
    let added = engine::models::register(cached.into_iter().map(|d| d.into_entry()));
    if added > 0 {
        app_log!("catalog: {added} cached entries newer than the built-in list");
    }
    // Shared with gpui-component, so its built-in strings follow the same locale.
    rust_i18n::set_locale(settings.ui_language.locale());

    // Starting the app again shows the running one's window instead.
    let messages = shell::messages();
    let instance = match shell::claim(&messages.0) {
        Ok(Some(instance)) => Some(instance),
        Ok(None) => {
            app_log!("already running: asked that instance to show its window");
            return;
        }
        Err(e) => {
            app_log!("cannot reach the running instance, or become it: {e}");
            None
        }
    };

    // The shell decides whether the last window closing quits the app: not when it stays
    // resident (on macOS too, where the Dock opens the window again).
    let app = gpui_kit::application()
        .with_assets(assets::AppAssets)
        .with_quit_mode(QuitMode::Explicit);
    app.on_reopen(|cx| shell::show(None, cx));
    gpui_kit::component::_rust_i18n_extend(_rust_i18n_backend(), "toolkit");
    app.run(move |cx| {
        gpui_kit::init(cx);
        view::bind_keys(cx);
        shell::init(custom_model, instance, messages, &settings, cx);
        #[cfg(target_os = "macos")]
        macos::set_menu(cx);
        shell::open_window(settings, None, cx).expect("failed to open the main window");
        cx.activate(true);
    });
}

/// Exercise the shipped native dependencies before settings, models or instance coordination.
#[cfg(target_os = "macos")]
fn package_smoke_test(pdf: &std::path::Path) -> Result<(), String> {
    // SAFETY: no threads have been started.
    unsafe { engine::llama::configure_metal() };
    let pdf = files::pdf::Pdf::read(pdf).map_err(|e| e.to_string())?;
    if pdf.units().is_empty() {
        return Err("PDFium returned no text from the smoke test PDF".into());
    }
    let runtime = engine::llama::Runtime::init(Default::default()).map_err(|e| e.to_string())?;
    // Hosted macOS runners may lack a Metal device. Report the actual backends enumerated.
    let devices = runtime.devices();
    if devices.is_empty() {
        return Err("the inference backend found no devices".into());
    }
    #[cfg(private_translate_bundled_metal)]
    if devices.iter().any(|device| device.name.starts_with("MTL")) {
        app_log!(
            "{}",
            macos::metal_shader_selection().ok_or("Metal shader selection was not reported")?
        );
    } else {
        app_log!("Metal shaders: unavailable (no Metal device)");
    }
    let backends: Vec<_> = devices
        .iter()
        .map(|device| device.backend.as_str())
        .collect();
    println!(
        "PDFium read the test PDF. Inference devices: {}.",
        backends.join(", ")
    );
    Ok(())
}
