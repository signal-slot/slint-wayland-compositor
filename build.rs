fn main() {
    // Compiled as a library, the .slint file can be imported as "@wayland_compositor" by
    // every crate that depends on this one: slint-build passes its location on through the
    // metadata of the `links` key. The generated code is included in `src/lib.rs`.
    let config = slint_build::CompilerConfiguration::new().as_library("wayland_compositor").rust_module("ui");
    slint_build::compile_with_config("ui/wayland-compositor.slint", config).unwrap();
}
