fn main() {
    println!("cargo:rerun-if-changed=build.rs");
    // The desktop app polls its whole future on the main thread, which
    // Windows gives only 1 MiB unless the executable header asks for more.
    // Ask for the 8 MiB Linux and macOS give it, as mj-cli/build.rs does.
    if std::env::var("CARGO_CFG_TARGET_OS").as_deref() == Ok("windows") {
        match std::env::var("CARGO_CFG_TARGET_ENV").as_deref() {
            Ok("msvc") => println!("cargo:rustc-link-arg-bins=/STACK:8388608"),
            Ok("gnu") => println!("cargo:rustc-link-arg-bins=-Wl,--stack,8388608"),
            _ => {}
        }
    }
}
