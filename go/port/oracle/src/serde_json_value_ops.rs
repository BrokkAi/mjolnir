//! Part of the oracle module `serde_json`: the operations named `value_*`,
//! which exercise the Value API.
//!
//! - `value_probe`: the input parsed as a Value, then `pointer`, `get` with a
//!   key and with an index, `[key]`, `[index]`, and the `as_*`/`is_*`
//!   accessors of what the pointer found.
//! - `value_from_value`: the input parsed as a Value, then `from_value` into
//!   the primitive `type`, and `to_value` of that.
//! - `value_index_mut`: `value[key] = 1` on the input parsed as a Value
//!   (which panics for some kinds), then the Value written.

use serde_json::{Value, json};

pub fn handle(request: &Value) -> Value {
    match crate::text(request, "op") {
        "value_probe" => probe(request),
        "value_from_value" => from_value(request),
        "value_index_mut" => index_mut(request),
        _ => crate::unknown(request),
    }
}

fn parsed(request: &Value) -> Value {
    serde_json::from_str(crate::text(request, "json")).expect("valid JSON")
}

fn probe(request: &Value) -> Value {
    let value = parsed(request);
    let pointer = crate::text(request, "pointer");
    let key = crate::text(request, "key");
    let index = request.get("index").and_then(Value::as_u64).unwrap_or(0) as usize;
    let found = value.pointer(pointer);
    json!({
        "pointer": found.map(|v| v.to_string()),
        "get_key": value.get(key).map(|v| v.to_string()),
        "get_index": value.get(index).map(|v| v.to_string()),
        "index_key": value[key].to_string(),
        "index_index": value[index].to_string(),
        "as_str": found.and_then(Value::as_str),
        "as_bool": found.and_then(Value::as_bool),
        "as_i64": found.and_then(Value::as_i64),
        "as_u64": found.and_then(Value::as_u64),
        "as_f64": found.and_then(Value::as_f64).map(|f| f.to_bits().to_string()),
        "is_null": found.map(Value::is_null),
        "is_number": found.map(Value::is_number),
        "is_i64": found.map(Value::is_i64),
        "is_u64": found.map(Value::is_u64),
        "is_f64": found.map(Value::is_f64),
        "is_array": found.map(Value::is_array),
        "is_object": found.map(Value::is_object),
        "display": value.to_string(),
        "alternate": format!("{value:#}"),
        "eq_self_reordered": reordered(&value) == value,
    })
}

/// The value with the members of every object in reverse order, which
/// serde_json's equality does not see.
fn reordered(value: &Value) -> Value {
    match value {
        Value::Object(map) => {
            let mut out = serde_json::Map::new();
            for (key, value) in map.iter().rev() {
                out.insert(key.clone(), reordered(value));
            }
            Value::Object(out)
        }
        Value::Array(items) => Value::Array(items.iter().map(reordered).collect()),
        other => other.clone(),
    }
}

fn from_value(request: &Value) -> Value {
    let value = parsed(request);
    fn run<T: serde::Serialize + for<'de> serde::Deserialize<'de>>(value: Value) -> Value {
        match serde_json::from_value::<T>(value) {
            Ok(result) => json!({"ok": serde_json::to_value(&result).map(|v| v.to_string()).unwrap()}),
            Err(error) => json!({"err": error.to_string()}),
        }
    }
    match crate::text(request, "type") {
        "bool" => run::<bool>(value),
        "u8" => run::<u8>(value),
        "i64" => run::<i64>(value),
        "u64" => run::<u64>(value),
        "f32" => run::<f32>(value),
        "f64" => run::<f64>(value),
        "String" => run::<String>(value),
        "char" => run::<char>(value),
        "unit" => run::<()>(value),
        "VecU8" => run::<Vec<u8>>(value),
        "OptionString" => run::<Option<String>>(value),
        "MapStringU8" => run::<std::collections::BTreeMap<String, u8>>(value),
        "MapU8Bool" => run::<std::collections::BTreeMap<u8, bool>>(value),
        "Tuple2" => run::<(u8, String)>(value),
        _ => crate::unknown(request),
    }
}

fn index_mut(request: &Value) -> Value {
    let mut value = parsed(request);
    let key = crate::text(request, "key");
    if let Some(index) = request.get("index").and_then(Value::as_u64) {
        value[index as usize] = json!(1);
    } else {
        value[key] = json!(1);
    }
    json!({"after": value.to_string()})
}
