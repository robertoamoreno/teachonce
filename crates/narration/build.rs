//! Link clang's compiler-rt on macOS.
//!
//! whisper.cpp's Metal backend guards newer APIs with `@available(...)`, which
//! clang lowers to calls to `___isPlatformVersionAtLeast` in compiler-rt. rustc
//! links with `-nodefaultlibs` and its own builtins, which do not include that
//! symbol, so a release build with a pinned deployment target (Tauri sets
//! `MACOSX_DEPLOYMENT_TARGET` from `minimumSystemVersion`) fails to link. The
//! dev build only gets away with it because nothing pins the target there.
//!
//! The archive lives in clang's resource directory, which differs between
//! Command Line Tools and Xcode installs, so it is looked up rather than
//! hard-coded. `rustc-link-lib` from a library's build script reaches the final
//! binary, so this covers the app, the tests and the examples alike.

fn main() {
    println!("cargo:rerun-if-changed=build.rs");
    if std::env::var("CARGO_CFG_TARGET_OS").as_deref() != Ok("macos") {
        return;
    }
    let Ok(output) = std::process::Command::new("clang").arg("--print-resource-dir").output() else {
        return;
    };
    let resource_dir = String::from_utf8_lossy(&output.stdout).trim().to_string();
    let darwin = std::path::Path::new(&resource_dir).join("lib").join("darwin");
    if darwin.join("libclang_rt.osx.a").is_file() {
        println!("cargo:rustc-link-search=native={}", darwin.display());
        println!("cargo:rustc-link-lib=static=clang_rt.osx");
    } else {
        println!(
            "cargo:warning=libclang_rt.osx.a not found under {}; a release build may fail to link ggml-metal",
            darwin.display()
        );
    }
}
