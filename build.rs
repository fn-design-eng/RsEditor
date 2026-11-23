fn main() {
    println!("cargo:rerun-if-changed=assets/icon.ico");
    println!("cargo:rerun-if-changed=build.rs");

    // Windows 向けビルドのときだけ exe にアイコンとバージョン情報を埋め込む
    #[cfg(windows)]
    {
        if std::env::var("CARGO_CFG_TARGET_OS").as_deref() == Ok("windows") {
            let mut res = winresource::WindowsResource::new();
            res.set_icon("assets/icon.ico");
            res.set("FileDescription", "RsEditor");
            res.set("ProductName", "RsEditor");
            if let Err(e) = res.compile() {
                println!("cargo:warning=failed to embed Windows resources: {e}");
            }
        }
    }
}
