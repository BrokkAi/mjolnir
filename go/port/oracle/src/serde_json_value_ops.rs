//! Part of the oracle module `serde_json`: the operations named `value_*`.

use serde_json::Value;

pub fn handle(request: &Value) -> Value {
    crate::unknown(request)
}
