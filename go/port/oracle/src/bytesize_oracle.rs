//! Oracle module `bytesize`. Requests are objects with an `"op"` member; see the
//! Go tests that use this module (`go/crates/bytesize/oracle_test.go`) for the
//! operations it serves.
//!
//! - `display`: `ByteSize(bytes)` printed every way the crate offers: `{}`,
//!   each `display()` style, `{:?}`, and each `as_*` conversion (as the bits
//!   of the f64, so that no JSON float formatting stands between the two
//!   sides).
//! - `format`: a value through one of a fixed list of format specs. `what`
//!   says which value: `size` (the `ByteSize`, the default), a `display()`
//!   style (`iec`, `iec_short`, `si`, `si_short`, `iec_bits`, `si_bits`),
//!   or `parse_error` (the error `text.parse::<ByteSize>()` returns, a
//!   `String`).
//! - `parse`: `text.parse::<ByteSize>()`: the byte count, or the error's
//!   Display and Debug text.
//! - `unit`: `text.parse::<Unit>()`: the unit's Debug name and factor, or the
//!   error's Display and Debug text.

use std::fmt::{Debug, Display};

use bytesize::{ByteSize, Unit};
use serde_json::{Value, json};

pub fn handle(request: &Value) -> Value {
    match crate::text(request, "op") {
        "display" => display(request),
        "format" => format(request),
        "parse" => parse(request),
        "unit" => unit(request),
        _ => crate::unknown(request),
    }
}

fn bytes(request: &Value) -> u64 {
    request.get("bytes").and_then(Value::as_u64).unwrap_or_default()
}

fn display(request: &Value) -> Value {
    let size = ByteSize(bytes(request));
    json!({
        "to_string": size.to_string(),
        "iec": size.display().iec().to_string(),
        "iec_short": size.display().iec_short().to_string(),
        "si": size.display().si().to_string(),
        "si_short": size.display().si_short().to_string(),
        "iec_bits": size.display().iec_bits().to_string(),
        "si_bits": size.display().si_bits().to_string(),
        "default_display": size.display().to_string(),
        "debug": format!("{:?}", size),
        "as_u64": size.as_u64(),
        "as_kb": size.as_kb().to_bits(),
        "as_kib": size.as_kib().to_bits(),
        "as_mb": size.as_mb().to_bits(),
        "as_mib": size.as_mib().to_bits(),
        "as_gb": size.as_gb().to_bits(),
        "as_gib": size.as_gib().to_bits(),
        "as_tb": size.as_tb().to_bits(),
        "as_tib": size.as_tib().to_bits(),
        "as_pb": size.as_pb().to_bits(),
        "as_pib": size.as_pib().to_bits(),
        "as_eb": size.as_eb().to_bits(),
        "as_eib": size.as_eib().to_bits(),
    })
}

/// The format specs the `format` operation knows. A spec reaches the value's
/// Display or Debug impl with its width, fill, alignment, precision and
/// flags.
fn format_with<T: Display + Debug>(spec: &str, value: &T) -> Option<String> {
    Some(match spec {
        "{}" => format!("{}", value),
        "{:10}" => format!("{:10}", value),
        "{:<10}" => format!("{:<10}", value),
        "{:>10}" => format!("{:>10}", value),
        "{:^10}" => format!("{:^10}", value),
        "{:->10}" => format!("{:->10}", value),
        "{:-<10}" => format!("{:-<10}", value),
        "{:-^10}" => format!("{:-^10}", value),
        "{:^11}" => format!("{:^11}", value),
        "{:3}" => format!("{:3}", value),
        "{:.0}" => format!("{:.0}", value),
        "{:.1}" => format!("{:.1}", value),
        "{:.2}" => format!("{:.2}", value),
        "{:.3}" => format!("{:.3}", value),
        "{:.5}" => format!("{:.5}", value),
        "{:.20}" => format!("{:.20}", value),
        "{:>12.5}" => format!("{:>12.5}", value),
        "{:<12.5}" => format!("{:<12.5}", value),
        "{:12.5}" => format!("{:12.5}", value),
        "{:-^15.5}" => format!("{:-^15.5}", value),
        "{:>10.0}" => format!("{:>10.0}", value),
        "{:3.5}" => format!("{:3.5}", value),
        "{:30.20}" => format!("{:30.20}", value),
        "{:012}" => format!("{:012}", value),
        "{:+}" => format!("{:+}", value),
        "{:#}" => format!("{:#}", value),
        "{:é^14}" => format!("{:é^14}", value),
        "{:🦀>12}" => format!("{:🦀>12}", value),
        "{:?}" => format!("{:?}", value),
        "{:#?}" => format!("{:#?}", value),
        "{:>40?}" => format!("{:>40?}", value),
        "{:.1?}" => format!("{:.1?}", value),
        _ => return None,
    })
}

fn format(request: &Value) -> Value {
    let size = ByteSize(bytes(request));
    let spec = crate::text(request, "spec");
    let formatted = match crate::text(request, "what") {
        "" | "size" => format_with(spec, &size),
        "iec" => format_with(spec, &size.display().iec()),
        "iec_short" => format_with(spec, &size.display().iec_short()),
        "si" => format_with(spec, &size.display().si()),
        "si_short" => format_with(spec, &size.display().si_short()),
        "iec_bits" => format_with(spec, &size.display().iec_bits()),
        "si_bits" => format_with(spec, &size.display().si_bits()),
        "parse_error" => match crate::text(request, "text").parse::<ByteSize>() {
            Ok(_) => return json!({"invalid": true}),
            Err(error) => format_with(spec, &error),
        },
        _ => None,
    };
    match formatted {
        Some(text) => json!({"text": text}),
        None => crate::unknown(request),
    }
}

fn parse(request: &Value) -> Value {
    match crate::text(request, "text").parse::<ByteSize>() {
        Ok(size) => json!({"ok": size.as_u64()}),
        Err(error) => json!({"err": error, "err_debug": format!("{:?}", error)}),
    }
}

fn unit(request: &Value) -> Value {
    match crate::text(request, "text").parse::<Unit>() {
        Ok(unit) => json!({"ok": format!("{:?}", unit), "factor": unit.clone() * 1u64}),
        Err(error) => json!({"err": error.to_string(), "err_debug": format!("{:?}", error)}),
    }
}
