use std::env;
use std::path::PathBuf;
use std::process::Command;

fn main() {
    println!("cargo:rerun-if-changed=build.rs");
    println!("cargo:rerun-if-changed=src/macos_speech.m");

    if env::var_os("CARGO_CFG_TARGET_OS").as_deref() != Some("macos".as_ref()) {
        return;
    }

    build_macos_speech_bridge();
}

fn build_macos_speech_bridge() {
    let out_dir = PathBuf::from(env::var_os("OUT_DIR").expect("OUT_DIR"));
    let object_path = out_dir.join("macos_speech.o");
    let library_path = out_dir.join("libmj_macos_speech.a");

    run(
        Command::new("xcrun")
            .arg("clang")
            .args([
                "-fobjc-arc",
                "-fblocks",
                "-fmodules",
                "-c",
                "src/macos_speech.m",
                "-o",
            ])
            .arg(&object_path),
        "compile macOS speech bridge",
    );

    run(
        Command::new("xcrun")
            .arg("libtool")
            .args(["-static", "-o"])
            .arg(&library_path)
            .arg(&object_path),
        "archive macOS speech bridge",
    );

    println!("cargo:rustc-link-search=native={}", out_dir.display());
    println!("cargo:rustc-link-lib=static=mj_macos_speech");
    println!("cargo:rustc-link-lib=framework=Foundation");
    println!("cargo:rustc-link-lib=framework=Speech");
    println!("cargo:rustc-link-lib=objc");
}

fn run(command: &mut Command, context: &str) {
    let status = command
        .status()
        .unwrap_or_else(|err| panic!("{context}: failed to launch command: {err}"));
    assert!(status.success(), "{context}: command exited with {status}");
}
