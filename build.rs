fn main() {
    let style = if std::env::var("CARGO_CFG_TARGET_OS").as_deref() == Ok("macos") { "cupertino" } else { "fluent" };
    let config = slint_build::CompilerConfiguration::new()
        .with_style(style.into())
        .with_bundled_translations("translations")
        .with_default_translation_context(slint_build::DefaultTranslationContext::None)
        // debug builds (cargo test) carry element info for the pointer-event UI tests; release builds stay lean
        .with_debug_info(std::env::var("PROFILE").as_deref() == Ok("debug"));
    slint_build::compile_with_config("ui/app.slint", config).unwrap();
}
