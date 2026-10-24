fn main() {
    println!("cargo:rerun-if-changed=assets/app.rc");
    println!("cargo:rerun-if-changed=assets/icon.ico");
    // 版本號與作者只寫在 Cargo.toml，這裡傳給資源檔，執行檔「詳細資料」才看得到。
    let env = |k: &str| std::env::var(k).unwrap_or_default();
    let macros = [
        format!("VER_MAJOR={}", env("CARGO_PKG_VERSION_MAJOR")),
        format!("VER_MINOR={}", env("CARGO_PKG_VERSION_MINOR")),
        format!("VER_PATCH={}", env("CARGO_PKG_VERSION_PATCH")),
        format!("VER_STR=\"{}\"", env("CARGO_PKG_VERSION")),
        format!("AUTHOR_STR=\"{}\"", env("CARGO_PKG_AUTHORS")),
        format!("DESC_STR=\"{}\"", env("CARGO_PKG_DESCRIPTION")),
    ];
    embed_resource::compile("assets/app.rc", &macros);
}
