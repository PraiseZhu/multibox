//! On Windows, builds the icon into the program. Explorer shows it from
//! there, and GPUI gives its windows the program's icon number 1, so the title
//! bar and the taskbar show it too. The name Task Manager shows comes along.
//!
//! macOS needs nothing here: the program hands the Dock its icon when it
//! starts. The files in `icons/` are drawn by the `icons` example.

fn main() {
    println!("cargo:rerun-if-changed=build.rs");
    println!("cargo:rerun-if-changed=icons/multibox.ico");
    // The resource compiler is a build dependency only on Windows hosts.
    #[cfg(windows)]
    if std::env::var("CARGO_CFG_TARGET_OS").as_deref() == Ok("windows") {
        windows_resources();
    }
}

#[cfg(windows)]
fn windows_resources() {
    use std::{env, fs, path::PathBuf};

    let var = |name: &str| env::var(name).unwrap_or_else(|_| panic!("cargo sets {name}"));
    let version = var("CARGO_PKG_VERSION");
    let numeric = format!(
        "{},{},{},0",
        var("CARGO_PKG_VERSION_MAJOR"),
        var("CARGO_PKG_VERSION_MINOR"),
        var("CARGO_PKG_VERSION_PATCH"),
    );
    let icons = PathBuf::from(var("CARGO_MANIFEST_DIR")).join("icons");
    // The icon is found through the include directory, so that no path, which
    // the resource compiler would read in the system's code page, goes into
    // the script. Until the `icons` example has drawn it, there is none.
    let icon = if icons.join("multibox.ico").is_file() {
        r#"1 ICON "multibox.ico""#
    } else {
        println!("cargo:warning=icons/multibox.ico is missing; run the `icons` example");
        ""
    };
    let script = format!(
        r#"{icon}

1 VERSIONINFO
FILEVERSION {numeric}
PRODUCTVERSION {numeric}
FILEOS 0x40004
FILETYPE 0x1
BEGIN
  BLOCK "StringFileInfo"
  BEGIN
    BLOCK "040904B0"
    BEGIN
      VALUE "FileDescription", "Multibox"
      VALUE "FileVersion", "{version}"
      VALUE "InternalName", "multibox-ui"
      VALUE "OriginalFilename", "multibox-ui.exe"
      VALUE "ProductName", "Multibox"
      VALUE "ProductVersion", "{version}"
    END
  END
  BLOCK "VarFileInfo"
  BEGIN
    VALUE "Translation", 0x409, 1200
  END
END
"#
    );
    let path = PathBuf::from(var("OUT_DIR")).join("multibox-ui.rc");
    fs::write(&path, script).expect("writing the resource script");
    embed_resource::compile(&path, embed_resource::ParamsIncludeDirs([icons]))
        .manifest_optional()
        .unwrap();
}
