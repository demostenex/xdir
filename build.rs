fn main() {
    // "fluent-dark" only to get a dark base palette for std-widgets; the
    // window/list colors below are still set explicitly.
    let config = slint_build::CompilerConfiguration::new().with_style("fluent-dark".into());
    slint_build::compile_with_config("ui/main.slint", config).unwrap();
}
