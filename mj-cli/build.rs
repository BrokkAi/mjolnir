fn main() {
    println!("cargo:rerun-if-changed=build.rs");
    // `mj` polls each command's whole future on the main thread. Linux and
    // macOS give that thread an 8 MiB stack, but Windows reserves only what
    // the executable header asks for, 1 MiB by default, and a debug
    // `daemon-run` overflows that before it serves. Ask for the same 8 MiB.
    // mj-desktop/build.rs does the same for the desktop app.
    if std::env::var("CARGO_CFG_TARGET_OS").as_deref() == Ok("windows") {
        match std::env::var("CARGO_CFG_TARGET_ENV").as_deref() {
            Ok("msvc") => println!("cargo:rustc-link-arg-bins=/STACK:8388608"),
            Ok("gnu") => println!("cargo:rustc-link-arg-bins=-Wl,--stack,8388608"),
            _ => {}
        }
    }
}
