//! Oracle module `serde_derive`: serde 1.0.229's derive behaviour, its data
//! model and serde_ignored 0.1.14, as go/crates/serde reproduces them.
//!
//! Requests are objects with an `"op"` member. The operations whose name
//! starts with `misc_` are served by `serde_derive_misc.rs`; the Go tests that
//! send each operation document it.

use serde_json::Value;

#[path = "serde_derive_misc.rs"]
mod misc;

pub fn handle(request: &Value) -> Value {
    let op = crate::text(request, "op");
    if op.starts_with("misc_") {
        return misc::handle(request);
    }
    crate::unknown(request)
}
