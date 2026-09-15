//! Static pdfium linking for the `pdfium` feature: without the feature
//! this script does nothing, and default builds are untouched. With it,
//! `PDFIUM_LIB_DIR` must name the directory holding the static archive
//! (`libpdfium.a` / `pdfium.lib`); pdfium's own releases ship only shared
//! libraries, but static archives are rebuilt from source by e.g.
//! https://github.com/kernoeb/pdfium-static (the workflow behind
//! https://github.com/bblanchon/pdfium-binaries with `build_type=static`).

use std::path::Path;

fn main() -> Result<(), Box<dyn std::error::Error>> {
    if std::env::var_os("CARGO_FEATURE_PDFIUM").is_none() {
        return Ok(());
    }
    println!("cargo:rerun-if-env-changed=PDFIUM_LIB_DIR");
    let dir = std::env::var_os("PDFIUM_LIB_DIR").ok_or(
        "the `pdfium` feature needs PDFIUM_LIB_DIR set to the directory holding \
         the static pdfium archive (libpdfium.a / pdfium.lib)",
    )?;
    let dir = Path::new(&dir);
    println!("cargo:rustc-link-search=native={}", dir.display());
    println!("cargo:rustc-link-lib=static=pdfium");
    // pdfium is C++: the archive's objects reference the C++ runtime, and
    // nothing else in aido's dependency tree pulls one in. Name the
    // runtime per target — libc++ on macOS, libstdc++ on Linux (verified
    // against libpdfium-linux-x64.a from kernoeb/pdfium-static
    // chromium/7985, which links with just `-lstdc++` added). MSVC links
    // its own runtime without help.
    match std::env::var("CARGO_CFG_TARGET_OS").as_deref() {
        Ok("macos") => println!("cargo:rustc-link-lib=dylib=c++"),
        Ok("linux") => println!("cargo:rustc-link-lib=dylib=stdc++"),
        _ => {}
    }
    // Replacing the archive in place does not touch the directory's
    // timestamp on every filesystem; watch the archive itself.
    for name in ["libpdfium.a", "pdfium.lib"] {
        let archive = dir.join(name);
        if archive.is_file() {
            println!("cargo:rerun-if-changed={}", archive.display());
        }
    }
    Ok(())
}
