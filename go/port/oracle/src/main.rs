//! Runs the real Rust code on inputs a Go test supplies.
//!
//! Usage: oracle <module>
//!
//! The program reads one JSON request per line on standard input and writes
//! one JSON reply per line on standard output, in order. A module decides what
//! its requests and replies look like; by convention a request is an object
//! with an `"op"` member. A request the module cannot serve is answered with
//! `{"oracle_error": "..."}` so the Go side fails loudly instead of comparing
//! against nothing.
//!
//! To add a module `foo`: add the feature `foo` (and its optional
//! dependencies) to Cargo.toml, create `src/foo.rs` with
//! `pub fn handle(request: &serde_json::Value) -> serde_json::Value`, and add
//! one line to the `modules!` list below.

use std::io::{BufRead, Write};

use serde_json::{Value, json};

/// Declares each module behind its feature and dispatches by name.
macro_rules! modules {
    ($($feature:literal => $module:ident),* $(,)?) => {
        $(#[cfg(feature = $feature)] mod $module;)*

        fn handler(name: &str) -> Option<fn(&Value) -> Value> {
            match name {
                $(#[cfg(feature = $feature)] $feature => Some($module::handle),)*
                _ => None,
            }
        }

        fn built() -> Vec<&'static str> {
            let mut names = Vec::new();
            $(#[cfg(feature = $feature)] names.push($feature);)*
            names
        }
    };
}

modules! {
    "std_fmt" => std_fmt,
    "std_str" => std_str,
    "std_collections" => std_collections,
    "std_path" => std_path,
    "std_io" => std_io,
    "std_time" => std_time,
    "std_process" => std_process,
    "std_sort" => std_sort,
    "serde_derive" => serde_derive,
    "serde_json" => serde_json_oracle,
}

/// The reply for a request a module does not understand.
#[allow(dead_code)]
pub fn unknown(request: &Value) -> Value {
    json!({ "oracle_error": format!("unknown request: {request}") })
}

/// A string member of a request, or an empty string.
#[allow(dead_code)]
pub fn text<'a>(request: &'a Value, key: &str) -> &'a str {
    request.get(key).and_then(Value::as_str).unwrap_or_default()
}

fn main() {
    let name = std::env::args().nth(1).unwrap_or_default();
    let Some(handle) = handler(&name) else {
        eprintln!(
            "oracle: module `{name}` is not in this build (built: {}). \
             Build with --features {name}.",
            built().join(", ")
        );
        std::process::exit(2);
    };
    let stdin = std::io::stdin();
    let stdout = std::io::stdout();
    let mut out = std::io::BufWriter::new(stdout.lock());
    for line in stdin.lock().lines() {
        let line = line.expect("read request");
        if line.trim().is_empty() {
            continue;
        }
        let reply = match serde_json::from_str::<Value>(&line) {
            Ok(request) => {
                // A panic in the code under test is a result worth comparing:
                // Go must panic with the same message.
                let caught = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| handle(&request)));
                match caught {
                    Ok(reply) => reply,
                    Err(payload) => {
                        let message = payload
                            .downcast_ref::<String>()
                            .cloned()
                            .or_else(|| payload.downcast_ref::<&str>().map(|text| (*text).to_owned()))
                            .unwrap_or_else(|| "non-string panic payload".to_owned());
                        json!({ "panic": message })
                    }
                }
            }
            Err(error) => json!({ "oracle_error": format!("request is not JSON: {error}") }),
        };
        serde_json::to_writer(&mut out, &reply).expect("write reply");
        out.write_all(b"\n").expect("write reply");
        // A live Go test waits for each reply before sending the next request.
        out.flush().expect("flush reply");
    }
}
