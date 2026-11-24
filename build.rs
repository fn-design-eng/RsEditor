// Windows 向けビルドのときだけ exe にアイコンとバージョン情報を埋め込む。
// ターゲットの判定はホストの cfg ではなく、実行時の CARGO_CFG_TARGET_OS で行う。
// リリースビルドでは埋め込みの失敗をエラーにして、アイコンやバージョン情報の
// 無い exe を配布してしまわないようにする (デバッグビルドは警告だけ)。

fn main() {
    println!("cargo:rerun-if-changed=assets/icon.ico");
    println!("cargo:rerun-if-changed=build.rs");

    if std::env::var("CARGO_CFG_TARGET_OS").as_deref() != Ok("windows") {
        return;
    }
    let release = std::env::var("PROFILE").as_deref() == Ok("release");
    embed_windows_resources(release);
}

/// Windows 以外のホストでは winresource (Windows 専用のビルド依存) が使えない。
#[cfg(not(windows))]
fn embed_windows_resources(release: bool) {
    let msg = "Windows resources (icon/version info) cannot be embedded when cross-compiling from a non-Windows host";
    if release {
        panic!("{msg}");
    }
    println!("cargo:warning={msg}");
}

#[cfg(windows)]
fn embed_windows_resources(release: bool) {
    let mut res = winresource::WindowsResource::new();
    res.set_icon("assets/icon.ico");
    res.set("FileDescription", "RsEditor");
    res.set("ProductName", "RsEditor");
    if let Err(e) = res.compile() {
        let msg = format!("failed to embed Windows resources: {e}");
        if release {
            panic!("{msg}");
        }
        println!("cargo:warning={msg}");
    }
}
