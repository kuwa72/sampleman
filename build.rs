fn main() {
    slint_build::compile("ui/main.slint").unwrap();

    if std::env::var("CARGO_CFG_TARGET_OS").unwrap_or_default() == "windows" {
        let mut res = winres::WindowsResource::new();
        res.set_icon("icons/icon.ico");
        #[cfg(not(windows))]
        {
            res.set_windres_path("x86_64-w64-mingw32-windres");
            res.set_ar_path("x86_64-w64-mingw32-ar");
        }
        res.compile().unwrap();

        if std::env::var("CARGO_CFG_TARGET_ENV").unwrap_or_default() != "msvc" {
            let out_dir = std::env::var("OUT_DIR").unwrap();
            println!(
                "cargo:rustc-link-arg-bins={}",
                std::path::Path::new(&out_dir).join("resource.o").display()
            );
        }
    }
}
