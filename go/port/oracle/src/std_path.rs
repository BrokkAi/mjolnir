//! Oracle module `std_path`. Requests are objects with an `"op"` member; see the
//! Go tests that use this module for the operations it serves.

use serde_json::Value;

pub fn handle(request: &Value) -> Value {
    crate::unknown(request)
}
