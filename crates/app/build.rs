fn main() {
    let config = slint_build::CompilerConfiguration::new().with_style("fluent-dark".into());
    slint_build::compile_with_config("ui/app.slint", config).expect("Slint UI failed to compile");

    // Icon and version details for niscord.exe (Explorer, taskbar, Properties).
    if std::env::var("CARGO_CFG_TARGET_OS").as_deref() == Ok("windows") {
        println!("cargo:rerun-if-changed=assets/icon.ico");
        let mut resource = winresource::WindowsResource::new();
        resource
            .set_icon("assets/icon.ico")
            .set("ProductName", "Niscord")
            .set("FileDescription", "Niscord: screen sharing with friends")
            .set("LegalCopyright", "MIT License");
        resource.compile().expect("embedding the Windows icon and version info failed");
    }
}
