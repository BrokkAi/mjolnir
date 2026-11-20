//! Oracle module `serde_json`: serde_json 1.0.150 (preserve_order,
//! float_roundtrip, raw_value), as go/crates/serdejson reproduces it.
//!
//! Requests are objects with an `"op"` member. The operations whose name
//! starts with `value_` are served by `serde_json_value_ops.rs`; the Go tests
//! that send each operation document it.

use serde_json::Value;

#[path = "serde_json_value_ops.rs"]
mod value_ops;

pub fn handle(request: &Value) -> Value {
    let op = crate::text(request, "op");
    if op.starts_with("value_") {
        return value_ops::handle(request);
    }
    crate::unknown(request)
}
