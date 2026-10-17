//! Embeds the brand icon and version metadata into the Windows `.exe`.
//!
//! The icon is generated from the same procedural renderer the window icon
//! uses (`theme::app_icon`); regenerate `assets/icon.ico` with:
//!     cargo test export_brand_assets -- --ignored

fn main() {
    println!("cargo:rerun-if-changed=assets/icon.ico");

    if std::env::var("CARGO_CFG_TARGET_OS").as_deref() != Ok("windows") {
        return;
    }

    let mut res = winresource::WindowsResource::new();
    if std::path::Path::new("assets/icon.ico").exists() {
        res.set_icon("assets/icon.ico");
    }
    res.set("ProductName", "Culvert");
    res.set("FileDescription", "Culvert - SSH client and tunnel manager");
    res.set("OriginalFilename", "culvert.exe");
    res.set("LegalCopyright", "MIT licensed");

    if let Err(err) = res.compile() {
        // Keep builds working on machines without the Windows resource
        // compiler; the window icon still comes from `theme::app_icon`.
        println!("cargo:warning=could not embed Windows resources ({err})");
    }
}
