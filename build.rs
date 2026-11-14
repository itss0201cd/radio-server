// build.rs — compile the spectrum visualiser to wasm and embed it.

use std::env;
use std::path::PathBuf;
use std::process::Command;

fn main() {
    println!("cargo:rerun-if-changed=build.rs");
    println!("cargo:rerun-if-changed=wasm/spectrum.rs");
    build_spectrum_wasm();
}

fn build_spectrum_wasm() {
    let rustc = env::var("RUSTC").unwrap_or_else(|_| "rustc".to_owned());
    let out =
        PathBuf::from(env::var("OUT_DIR").expect("OUT_DIR is set by cargo")).join("spectrum.wasm");

    let output = Command::new(rustc)
        .args([
            "--crate-name",
            "spectrum",
            "--edition",
            "2024",
            "--target",
            "wasm32-unknown-unknown",
            "--crate-type",
            "cdylib",
            "-C",
            "opt-level=3",
            "-C",
            "panic=abort",
            "-C",
            "codegen-units=1",
            "-C",
            "lto=yes",
            "-C",
            "strip=symbols",
            "wasm/spectrum.rs",
            "-o",
        ])
        .arg(&out)
        .output()
        .unwrap_or_else(|error| panic!("could not run rustc for spectrum.wasm: {error}"));
    if !output.status.success() {
        panic!(
            "spectrum.wasm build failed:\n{}",
            String::from_utf8_lossy(&output.stderr)
        );
    }
}
