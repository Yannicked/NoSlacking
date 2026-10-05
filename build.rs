//! Compiles the translations in `assets/i18n` into the binary, and for
//! Windows embeds the app icon and version info in the executable.

/// Shape checks for the catalogs, shared with the tests in `src/i18n.rs`.
#[path = "build/po.rs"]
mod po;

fn main() {
    check_catalogs("assets/i18n");
    fastframe_i18n::build::compile_catalogs("assets/i18n");
    windows_resources();
}

/// Stops the build on a catalog the compiler would read wrongly without
/// saying so: a `#` comment glued to the entry above it merges the two
/// entries, and one translation quietly falls back to English.
fn check_catalogs(directory: &str) {
    println!("cargo:rerun-if-changed=build/po.rs");
    let entries = match std::fs::read_dir(directory) {
        Ok(entries) => entries,
        // The compiler reports a missing folder in its own words.
        Err(_) => return,
    };
    let mut problems = Vec::new();
    for path in entries.flatten().map(|entry| entry.path()) {
        if path.extension().is_none_or(|e| e != "po") {
            continue;
        }
        let text = match std::fs::read_to_string(&path) {
            Ok(text) => text,
            Err(error) => panic!("could not read {}: {error}", path.display()),
        };
        for line in po::glued_comments(&text) {
            problems.push(format!(
                "{}:{line}: a `#` comment follows an entry with no blank line; \
                 put an empty line before it, or it merges into the entry above",
                path.display()
            ));
        }
    }
    if !problems.is_empty() {
        panic!("{}", problems.join("\n"));
    }
}

/// Explorer, the taskbar and the Start menu read the icon from the .exe.
/// Keyed on the target, not the host, so a cross build gets it too when
/// `llvm-rc` is around. A missing resource compiler only costs the icon,
/// never the build.
fn windows_resources() {
    if std::env::var("CARGO_CFG_TARGET_OS").as_deref() != Ok("windows") {
        return;
    }
    println!("cargo:rerun-if-changed=packaging/windows/noslacking.ico");
    let mut resource = winresource::WindowsResource::new();
    resource
        .set_icon("packaging/windows/noslacking.ico")
        .set("ProductName", "NoSlacking")
        .set("FileDescription", "NoSlacking")
        .set("LegalCopyright", "Copyright (c) 2026 Yannick de Jong (MIT)");
    if let Err(error) = resource.compile() {
        println!("cargo:warning=no Windows icon embedded: {error}");
    }
}
