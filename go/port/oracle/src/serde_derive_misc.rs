//! Part of the oracle module `serde_derive`: the operations named `misc_*`.

use serde_json::Value;

pub fn handle(request: &Value) -> Value {
    crate::unknown(request)
}
