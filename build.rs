fn main() {
    println!("cargo:rerun-if-changed=build.rs");
    // The distributed GUI always requests elevation; debug builds can inspect history without UAC.
    if std::env::var("PROFILE").as_deref() == Ok("release") {
        println!("cargo:rustc-link-arg-bin=netmon=/MANIFEST:EMBED");
        println!(
            "cargo:rustc-link-arg-bin=netmon=/MANIFESTUAC:level='requireAdministrator' uiAccess='false'"
        );
    }
}
