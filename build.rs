fn main() {
    println!("cargo:rerun-if-changed=assets/blurman.ico");
    if std::env::var("CARGO_CFG_TARGET_OS").ok().as_deref() != Some("windows") {
        return;
    }
    let icon = std::path::Path::new(&std::env::var("CARGO_MANIFEST_DIR").unwrap()).join("assets/blurman.ico");
    let mut res = winresource::WindowsResource::new();
    res.set_icon(icon.to_str().expect("icon path"));
    res.compile().expect("embed the app icon");
}
