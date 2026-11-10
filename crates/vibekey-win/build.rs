use std::env;
use std::fs;
use std::path::PathBuf;

fn main() {
    println!("cargo:rerun-if-changed=assets/icon.ico");
    if env::var("CARGO_CFG_TARGET_OS").as_deref() != Ok("windows") {
        return;
    }

    let manifest_dir = PathBuf::from(env::var_os("CARGO_MANIFEST_DIR").unwrap());
    let icon = manifest_dir.join("assets/icon.ico");
    let icon = icon.to_string_lossy().replace('\\', "/");
    let out_dir = PathBuf::from(env::var_os("OUT_DIR").unwrap());
    let resource = out_dir.join("vibekey.rc");
    let major = env::var("CARGO_PKG_VERSION_MAJOR").unwrap();
    let minor = env::var("CARGO_PKG_VERSION_MINOR").unwrap();
    let patch = env::var("CARGO_PKG_VERSION_PATCH").unwrap();
    let version = env::var("CARGO_PKG_VERSION").unwrap();
    let script = format!(
        r#"#pragma code_page(65001)
1 ICON "{icon}"
1 VERSIONINFO
FILEVERSION {major},{minor},{patch},0
PRODUCTVERSION {major},{minor},{patch},0
FILEFLAGS 0
FILEOS 0x40004
FILETYPE 0x1
BEGIN
    BLOCK "StringFileInfo"
    BEGIN
        BLOCK "080404b0"
        BEGIN
            VALUE "FileDescription", "Vibekey 文本片段输入工具"
            VALUE "ProductName", "Vibekey"
            VALUE "FileVersion", "{version}"
            VALUE "ProductVersion", "{version}"
            VALUE "OriginalFilename", "vibekey.exe"
            VALUE "LegalCopyright", "MIT License © Vibekey contributors"
        END
    END
    BLOCK "VarFileInfo"
    BEGIN
        VALUE "Translation", 0x804, 1200
    END
END
"#,
    );
    fs::write(&resource, script).expect("write generated Windows resource script");
    let resource = resource.to_string_lossy().replace('\\', "/");
    // Icon/version metadata is cosmetic: unsupported cross toolchains should
    // still be able to build the functional application.
    embed_resource::compile(&resource, embed_resource::NONE);
}
