use std::{env, fs, path::PathBuf};

fn main() {
    slint_build::compile("ui/app.slint").expect("failed to compile Slint UI");

    let manifest_dir = PathBuf::from(env::var("CARGO_MANIFEST_DIR").unwrap());
    let mpv_dir = manifest_dir.join("third_party").join("mpv-dev").join("64");

    // libmpv2-sys emits `cargo:rustc-link-lib=mpv` but never a search path, so the
    // location of the import library is entirely our responsibility.
    println!("cargo:rustc-link-search=native={}", mpv_dir.display());

    // Put libmpv-2.dll next to the executable so `cargo run` works without the DLL
    // being on PATH. OUT_DIR is target/<profile>/build/<pkg>-<hash>/out.
    let out_dir = PathBuf::from(env::var("OUT_DIR").unwrap());
    if let Some(profile_dir) = out_dir.ancestors().nth(3) {
        let dll = mpv_dir.join("libmpv-2.dll");
        if dll.exists() {
            let _ = fs::copy(&dll, profile_dir.join("libmpv-2.dll"));
        }
    }

    println!("cargo:rerun-if-changed=build.rs");
    println!("cargo:rerun-if-changed=ui");
    // Relink when the import library or DLL is regenerated.
    println!(
        "cargo:rerun-if-changed={}",
        mpv_dir.join("mpv.lib").display()
    );
    println!(
        "cargo:rerun-if-changed={}",
        mpv_dir.join("libmpv-2.dll").display()
    );
}
