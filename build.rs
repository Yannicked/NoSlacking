//! Compiles the translations in `assets/i18n` into the binary.

fn main() {
    fastframe_i18n::build::compile_catalogs("assets/i18n");
}
