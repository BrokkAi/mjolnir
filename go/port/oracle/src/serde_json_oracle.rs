//! Oracle module `serde_json`: serde_json 1.0.150 (preserve_order,
//! float_roundtrip, raw_value), as go/crates/serdejson reproduces it.
//!
//! Requests are objects with an `"op"` member. The operations whose name
//! starts with `value_` are served by `serde_json_value_ops.rs`; the Go tests
//! that send each operation document it (go/crates/serdejson/oracle_test.go).
//!
//! An input is `json` (text) or `bytes` (numbers); a float is given by its
//! bits as a decimal string, so that JSON numbers cannot change it.
//!
//! - `parse`: the input as a Value (mode str, slice or reader), written
//!   compact and pretty; or the error's text, category and position.
//! - `prim`: the input as the primitive `type` (bool, i8 .. u64, f32, f64,
//!   char, String, unit), written back; or the error.
//! - `f64_format`, `f32_format`: serde_json's text of the float with `bits`,
//!   and the invalid_type error that shows how its errors print it.
//! - `string`: the input bytes, which are UTF-8, as a JSON string.
//! - `stream`: the values of a StreamDeserializer over the input, at most
//!   ten, each with the byte offset after it.
//! - `raw`: RawValue::from_string of the input, and a struct holding it.
//! - `number`: Number::from_str of the input.

use serde::{Deserialize, Serialize};
use serde_json::{Value, json};

#[path = "serde_json_value_ops.rs"]
mod value_ops;

pub fn handle(request: &Value) -> Value {
    let op = crate::text(request, "op");
    if op.starts_with("value_") {
        return value_ops::handle(request);
    }
    match op {
        "parse" => parse(request),
        "prim" => prim(request),
        "f64_format" => f64_format(request),
        "f32_format" => f32_format(request),
        "string" => string(request),
        "stream" => stream(request),
        "raw" => raw(request),
        "number" => number(request),
        _ => crate::unknown(request),
    }
}

/// The input of a request: `json` as text, or `bytes` as numbers.
pub fn input(request: &Value) -> Vec<u8> {
    if let Some(bytes) = request.get("bytes").and_then(Value::as_array) {
        return bytes.iter().map(|b| b.as_u64().unwrap_or(0) as u8).collect();
    }
    crate::text(request, "json").as_bytes().to_vec()
}

pub fn error_reply(error: &serde_json::Error) -> Value {
    json!({
        "err": error.to_string(),
        "class": format!("{:?}", error.classify()),
        "line": error.line(),
        "column": error.column(),
    })
}

fn from_input<T: for<'de> Deserialize<'de>>(request: &Value) -> Result<T, Value> {
    let input = input(request);
    let result = match crate::text(request, "mode") {
        "slice" => serde_json::from_slice(&input),
        "reader" => serde_json::from_reader(&input[..]),
        _ => match std::str::from_utf8(&input) {
            Ok(text) => serde_json::from_str(text),
            Err(_) => return Err(json!({"oracle_error": "str input is not UTF-8"})),
        },
    };
    result.map_err(|error| error_reply(&error))
}

fn written<T: Serialize>(value: &T) -> Value {
    let compact = match serde_json::to_string(value) {
        Ok(text) => json!(text),
        Err(error) => json!({"ser_err": error.to_string()}),
    };
    let pretty = match serde_json::to_string_pretty(value) {
        Ok(text) => json!(text),
        Err(error) => json!({"ser_err": error.to_string()}),
    };
    json!({"ok": {"compact": compact, "pretty": pretty}})
}

fn parse(request: &Value) -> Value {
    match from_input::<Value>(request) {
        Ok(value) => written(&value),
        Err(reply) => reply,
    }
}

fn prim_of<T: Serialize + for<'de> Deserialize<'de>>(request: &Value) -> Value {
    match from_input::<T>(request) {
        Ok(value) => written(&value),
        Err(reply) => reply,
    }
}

fn prim(request: &Value) -> Value {
    match crate::text(request, "type") {
        "bool" => prim_of::<bool>(request),
        "i8" => prim_of::<i8>(request),
        "i16" => prim_of::<i16>(request),
        "i32" => prim_of::<i32>(request),
        "i64" => prim_of::<i64>(request),
        "u8" => prim_of::<u8>(request),
        "u16" => prim_of::<u16>(request),
        "u32" => prim_of::<u32>(request),
        "u64" => prim_of::<u64>(request),
        "f32" => prim_of::<f32>(request),
        "f64" => prim_of::<f64>(request),
        "char" => prim_of::<char>(request),
        "String" => prim_of::<String>(request),
        "unit" => prim_of::<()>(request),
        "OptionU8" => prim_of::<Option<u8>>(request),
        "VecString" => prim_of::<Vec<String>>(request),
        "Map" => prim_of::<serde_json::Map<String, Value>>(request),
        _ => crate::unknown(request),
    }
}

fn bits(request: &Value) -> u64 {
    crate::text(request, "bits").parse().unwrap_or(0)
}

fn f64_format(request: &Value) -> Value {
    let value = f64::from_bits(bits(request));
    let unexpected = <serde_json::Error as serde::de::Error>::invalid_type(
        serde::de::Unexpected::Float(value),
        &"a boolean",
    )
    .to_string();
    let as_value = serde_json::to_value(value).map(|v| v.to_string()).ok();
    let key = serde_json::to_string(&std::collections::BTreeMap::from([(
        ordered_key(value),
        0,
    )]))
    .map_err(|error| error.to_string());
    json!({
        "text": serde_json::to_string(&value).unwrap(),
        "value": as_value,
        "unexpected": unexpected,
        "key": match key { Ok(text) => json!(text), Err(error) => json!({"err": error}) },
        "display": format!("{}", serde_json::Number::from_f64(value).map(|n| n.to_string()).unwrap_or_default()),
    })
}

/// A float map key: serde_json writes it quoted, and refuses NaN and the
/// infinities.
#[derive(PartialEq, PartialOrd)]
struct FloatKey(f64);

impl Eq for FloatKey {}

impl Ord for FloatKey {
    fn cmp(&self, other: &Self) -> std::cmp::Ordering {
        self.0.total_cmp(&other.0)
    }
}

impl Serialize for FloatKey {
    fn serialize<S: serde::Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        serializer.serialize_f64(self.0)
    }
}

fn ordered_key(value: f64) -> FloatKey {
    FloatKey(value)
}

fn f32_format(request: &Value) -> Value {
    let value = f32::from_bits(bits(request) as u32);
    let as_value = serde_json::to_value(value).map(|v| v.to_string()).ok();
    json!({
        "text": serde_json::to_string(&value).unwrap(),
        "value": as_value,
    })
}

fn string(request: &Value) -> Value {
    let input = input(request);
    let text = String::from_utf8(input).expect("UTF-8 input");
    let compact = serde_json::to_string(&text).unwrap();
    let pretty = serde_json::to_string_pretty(&vec![text.clone()]).unwrap();
    let back: String = serde_json::from_str(&compact).unwrap();
    json!({"compact": compact, "pretty": pretty, "back": back == text})
}

fn stream(request: &Value) -> Value {
    let input = input(request);
    let mut items = Vec::new();
    let mode = crate::text(request, "mode");
    let mut push = |item: Option<Result<Value, serde_json::Error>>, offset: usize| -> bool {
        match item {
            None => {
                items.push(json!({"end": offset}));
                false
            }
            Some(Ok(value)) => {
                items.push(json!({"value": value.to_string(), "offset": offset}));
                true
            }
            Some(Err(error)) => {
                let mut reply = error_reply(&error);
                reply["offset"] = json!(offset);
                items.push(reply);
                true
            }
        }
    };
    if mode == "reader" {
        let mut iter = serde_json::Deserializer::from_reader(&input[..]).into_iter::<Value>();
        for _ in 0..10 {
            let item = iter.next();
            if !push(item, iter.byte_offset()) {
                break;
            }
        }
    } else {
        let mut iter = serde_json::Deserializer::from_slice(&input).into_iter::<Value>();
        for _ in 0..10 {
            let item = iter.next();
            if !push(item, iter.byte_offset()) {
                break;
            }
        }
    }
    json!(items)
}

#[derive(Serialize, Deserialize)]
struct RawHolder {
    raw: Box<serde_json::value::RawValue>,
}

fn raw(request: &Value) -> Value {
    let text = String::from_utf8(input(request)).expect("UTF-8 input");
    match serde_json::value::RawValue::from_string(text.clone()) {
        Ok(raw) => {
            let holder = RawHolder { raw };
            let compact = serde_json::to_string(&holder).unwrap();
            let pretty = serde_json::to_string_pretty(&holder).unwrap();
            let as_value = serde_json::to_value(&holder)
                .map(|v| v.to_string())
                .map_err(|error| error.to_string());
            json!({
                "get": holder.raw.get(),
                "compact": compact,
                "pretty": pretty,
                "value": match as_value { Ok(text) => json!(text), Err(error) => json!({"err": error}) },
            })
        }
        Err(error) => error_reply(&error),
    }
}

fn number(request: &Value) -> Value {
    let text = String::from_utf8(input(request)).expect("UTF-8 input");
    match text.parse::<serde_json::Number>() {
        Ok(number) => json!({
            "display": number.to_string(),
            "is_i64": number.is_i64(),
            "is_u64": number.is_u64(),
            "is_f64": number.is_f64(),
        }),
        Err(error) => error_reply(&error),
    }
}
