//! Embeds the pre-built web application into the crate.
//!
//! `brokk-mj-server` is published to crates.io and built on Linux, macOS, and
//! Windows release jobs that have no Node toolchain, so this script is pure
//! Rust over `std::fs`: it never shells out to npm or Vite. The compiled SPA
//! is checked in under `src/web/dist/`; rebuilding it is `cd web && npm run
//! build`, never a Cargo concern.

use std::env;
use std::fs;
use std::path::{Path, PathBuf};

fn main() {
    let manifest_dir = PathBuf::from(env::var("CARGO_MANIFEST_DIR").expect("Cargo sets it"));
    let dist = manifest_dir.join("src/web/dist");
    println!("cargo:rerun-if-changed={}", dist.display());

    let mut files = Vec::new();
    walk(&dist, &dist, &mut files);
    files.sort();

    assert!(
        files.iter().any(|path| path == "index.html"),
        "src/web/dist/index.html is missing: build the web application first \
         (cd web && npm run build)"
    );

    let mut generated = String::from(
        "/// How long a browser may hold each embedded asset.\n\
         #[derive(Clone, Copy, Debug, PartialEq, Eq)]\n\
         pub(super) enum AssetCache {\n\
         \x20   Immutable,\n\
         \x20   NoCache,\n\
         \x20   NoStore,\n\
         }\n\n\
         /// One file from `src/web/dist`, keyed by its absolute URL path.\n\
         pub(super) struct EmbeddedAsset {\n\
         \x20   pub(super) path: &'static str,\n\
         \x20   pub(super) content_type: &'static str,\n\
         \x20   pub(super) cache: AssetCache,\n\
         \x20   pub(super) body: &'static [u8],\n\
         }\n\n\
         pub(super) static EMBEDDED_ASSETS: &[EmbeddedAsset] = &[\n",
    );
    for file in &files {
        let url_path = format!("/{file}");
        // `concat!` + forward slashes keep the path valid on Windows, where
        // the manifest directory itself contains backslashes.
        generated.push_str(&format!(
            "    EmbeddedAsset {{\n\
             \x20       path: {:?},\n\
             \x20       content_type: {:?},\n\
             \x20       cache: AssetCache::{},\n\
             \x20       body: include_bytes!(concat!(\n\
             \x20           env!(\"CARGO_MANIFEST_DIR\"),\n\
             \x20           \"/src/web/dist/\",\n\
             \x20           {:?},\n\
             \x20       )),\n\
             \x20   }},\n",
            url_path,
            content_type(file),
            cache_policy(&url_path),
            file,
        ));
    }
    generated.push_str(
        "];\n\n\
         /// The embedded asset served at `path` (`/assets/index-….js`), if the\n\
         /// build that produced this binary shipped one.\n\
         pub(super) fn lookup_asset(path: &str) -> Option<&'static EmbeddedAsset> {\n\
         \x20   EMBEDDED_ASSETS.iter().find(|asset| asset.path == path)\n\
         }\n",
    );

    let out_dir = PathBuf::from(env::var("OUT_DIR").expect("Cargo sets it"));
    fs::write(out_dir.join("web_assets.rs"), generated).expect("write generated web_assets.rs");
}

/// Collect every file under `directory` as a forward-slash path relative to
/// `root`, and tell Cargo to rebuild when any directory or file changes.
fn walk(root: &Path, directory: &Path, files: &mut Vec<String>) {
    let mut entries: Vec<_> = fs::read_dir(directory)
        .unwrap_or_else(|error| panic!("cannot read {}: {error}", directory.display()))
        .map(|entry| entry.expect("read a directory entry").path())
        .collect();
    entries.sort();
    for entry in entries {
        println!("cargo:rerun-if-changed={}", entry.display());
        if entry.is_dir() {
            walk(root, &entry, files);
        } else {
            files.push(
                entry
                    .strip_prefix(root)
                    .expect("walked entries stay under the root")
                    .to_str()
                    .expect("asset names are UTF-8")
                    .replace('\\', "/"),
            );
        }
    }
}

fn content_type(file: &str) -> &'static str {
    match Path::new(file)
        .extension()
        .and_then(|extension| extension.to_str())
    {
        Some("html") => "text/html; charset=utf-8",
        Some("js" | "mjs") => "text/javascript; charset=utf-8",
        Some("css") => "text/css; charset=utf-8",
        Some("svg") => "image/svg+xml",
        Some("png") => "image/png",
        Some("woff2") => "font/woff2",
        Some("webmanifest") => "application/manifest+json",
        Some("json") => "application/json",
        Some("ico") => "image/x-icon",
        _ => "application/octet-stream",
    }
}

/// Vite content-hashes everything under `/assets/`, so those files are
/// immutable forever. The service worker is the one asset that can never be
/// fixed by a later upgrade, so it is never stored. Everything else (the HTML
/// shell, icons, the manifest) revalidates: `no-cache` means "ask first", so
/// an upgraded application is picked up on the next load while an unchanged
/// one costs one conditional request.
fn cache_policy(url_path: &str) -> &'static str {
    if url_path.starts_with("/assets/") {
        "Immutable"
    } else if url_path == "/service-worker.js" {
        "NoStore"
    } else {
        "NoCache"
    }
}
