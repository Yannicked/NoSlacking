//! Compiles the translations in `assets/i18n` into the binary, and for
//! Windows embeds the app icon and version info in the executable.

fn main() {
    fastframe_i18n::build::compile_catalogs("assets/i18n");
    windows_resources();
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
