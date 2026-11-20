//! Oracle module `std_str`: Rust's `str`, `String` and `char` methods, for the
//! Go package `go/rs/strs`.
//!
//! Requests are objects with an `"op"` member. Strings travel as JSON strings,
//! byte strings (which may hold invalid UTF-8) as lowercase hex, chars as code
//! points, and 64-bit and wider integers as decimal strings. The Go tests that
//! mirror the replies are `go/rs/strs/oracle_test.go`, `oracle_more_test.go`
//! and `oracle_fmt_test.go`.

use std::fmt::{Debug, Display};

use serde_json::{Map, Value, json};

pub fn handle(request: &Value) -> Value {
    match crate::text(request, "op") {
        "from_utf8" => from_utf8(request),
        "utf16" => utf16(request),
        "char_class" => char_class(request),
        "char_case" => char_case(request),
        "char_misc" => char_misc(request),
        "digit" => digit(request),
        "str" => str_all(request),
        "index" => index(request),
        "slice" => slice(request),
        "split_at" => split_at(request),
        "pat" => pattern(request),
        "string" => string_mut(request),
        "parse" => parse(request),
        "radix" => radix(request),
        "pair" => pair(request),
        "bytes" => bytes_ops(request),
        "repeat" => json!(crate::text(request, "s").repeat(num(request, "n"))),
        "fmt" => fmt(request),
        _ => crate::unknown(request),
    }
}

fn num(request: &Value, key: &str) -> usize {
    request.get(key).and_then(Value::as_u64).unwrap_or(0) as usize
}

fn hex(bytes: &[u8]) -> String {
    bytes.iter().map(|byte| format!("{byte:02x}")).collect()
}

fn unhex(text: &str) -> Vec<u8> {
    (0..text.len() / 2).map(|i| u8::from_str_radix(&text[2 * i..2 * i + 2], 16).unwrap()).collect()
}

fn bytes_of(request: &Value, key: &str) -> Vec<u8> {
    unhex(crate::text(request, key))
}

fn chr(value: &Value) -> char {
    char::from_u32(value.as_u64().unwrap() as u32).expect("request holds a char")
}

fn chars_of(value: &Value) -> Vec<char> {
    value.as_array().map(|items| items.iter().map(chr).collect()).unwrap_or_default()
}

fn codes(text: impl IntoIterator<Item = char>) -> Vec<u32> {
    text.into_iter().map(|c| c as u32).collect()
}

/// `{"ok": ..}` or `{"err": <Display>, "debug": <Debug>}`.
fn result<T: Into<Value>, E: Display + Debug>(result: Result<T, E>) -> Value {
    match result {
        Ok(value) => json!({ "ok": value.into() }),
        Err(error) => json!({ "err": error.to_string(), "debug": format!("{error:?}") }),
    }
}

fn from_utf8(request: &Value) -> Value {
    let bytes = bytes_of(request, "b");
    let str_result = match std::str::from_utf8(&bytes) {
        Ok(text) => json!({ "ok": text }),
        Err(error) => json!({
            "err": error.to_string(),
            "debug": format!("{error:?}"),
            "valid_up_to": error.valid_up_to(),
            "error_len": error.error_len(),
        }),
    };
    let string_result = match String::from_utf8(bytes.clone()) {
        Ok(text) => json!({ "ok": text }),
        Err(error) => json!({
            "err": error.to_string(),
            "debug": format!("{error:?}"),
            "bytes": hex(error.as_bytes()),
            "valid_up_to": error.utf8_error().valid_up_to(),
            "error_len": error.utf8_error().error_len(),
        }),
    };
    let chunks: Vec<Value> =
        bytes.utf8_chunks().map(|chunk| json!([chunk.valid(), hex(chunk.invalid())])).collect();
    json!({
        "str": str_result,
        "string": string_result,
        "lossy": String::from_utf8_lossy(&bytes),
        "chunks": chunks,
    })
}

/// The UTF-16 code units in `"u"`.
fn units_of(request: &Value) -> Vec<u16> {
    request["u"]
        .as_array()
        .map(|items| items.iter().map(|item| item.as_u64().unwrap() as u16).collect())
        .unwrap_or_default()
}

fn utf16(request: &Value) -> Value {
    let units = units_of(request);
    json!({
        "from_utf16": result(String::from_utf16(&units)),
        "lossy": String::from_utf16_lossy(&units),
    })
}

fn class_mask(c: char) -> u32 {
    let continued: String = format!("a{c}").escape_debug().skip(1).collect();
    let flags = [
        c.is_alphabetic(),
        c.is_numeric(),
        c.is_alphanumeric(),
        c.is_whitespace(),
        c.is_lowercase(),
        c.is_uppercase(),
        c.is_control(),
        c.is_ascii(),
        c.is_ascii_alphabetic(),
        c.is_ascii_digit(),
        c.is_ascii_alphanumeric(),
        c.is_ascii_hexdigit(),
        c.is_ascii_punctuation(),
        c.is_ascii_graphic(),
        c.is_ascii_whitespace(),
        c.is_ascii_control(),
        c.is_ascii_uppercase(),
        c.is_ascii_lowercase(),
        // Whether each escaping form changes the char: escape_debug on its
        // own, escape_debug after another char (grapheme extenders are kept),
        // and escape_default.
        c.escape_debug().count() != 1,
        continued.chars().count() != 1,
        c.escape_default().count() != 1,
    ];
    flags.iter().enumerate().fold(0, |mask, (bit, &set)| mask | (u32::from(set) << bit))
}

/// The class mask of every code point in `start..end`, run-length encoded as
/// `[mask, count, mask, count, ..]`. A surrogate has mask 0.
fn char_class(request: &Value) -> Value {
    let (start, end) = (num(request, "start") as u32, num(request, "end") as u32);
    let mut runs: Vec<u32> = Vec::new();
    for code in start..end {
        let mask = char::from_u32(code).map_or(0, class_mask);
        match runs.len() {
            n if n >= 2 && runs[n - 2] == mask => runs[n - 1] += 1,
            _ => runs.extend([mask, 1]),
        }
    }
    json!(runs)
}

/// Every char in `start..end` whose lowercase or uppercase mapping is not
/// itself: `[code, [lower..], [upper..], to_ascii_lowercase, to_ascii_uppercase]`.
fn char_case(request: &Value) -> Value {
    let (start, end) = (num(request, "start") as u32, num(request, "end") as u32);
    let mut changed = Vec::new();
    for c in (start..end).filter_map(char::from_u32) {
        let lower = codes(c.to_lowercase());
        let upper = codes(c.to_uppercase());
        if lower != [c as u32] || upper != [c as u32] {
            changed.push(json!([
                c as u32,
                lower,
                upper,
                c.to_ascii_lowercase() as u32,
                c.to_ascii_uppercase() as u32
            ]));
        }
    }
    json!(changed)
}

fn char_misc(request: &Value) -> Value {
    let replies: Vec<Value> = chars_of(&request["cs"])
        .into_iter()
        .map(|c| {
            let mut utf8 = [0u8; 4];
            let mut utf16 = [0u16; 2];
            json!({
                "len_utf8": c.len_utf8(),
                "len_utf16": c.len_utf16(),
                "utf8": hex(c.encode_utf8(&mut utf8).as_bytes()),
                "utf16": c.encode_utf16(&mut utf16).to_vec(),
                "escape_debug": c.escape_debug().to_string(),
                "escape_default": c.escape_default().to_string(),
                "escape_unicode": c.escape_unicode().to_string(),
                "debug": format!("{c:?}"),
                "lower": codes(c.to_lowercase()),
                "upper": codes(c.to_uppercase()),
                "ascii_lower": c.to_ascii_lowercase() as u32,
                "ascii_upper": c.to_ascii_uppercase() as u32,
            })
        })
        .collect();
    json!(replies)
}

fn digit(request: &Value) -> Value {
    let radix = num(request, "radix") as u32;
    match crate::text(request, "f") {
        "to_digit" => json!(
            chars_of(&request["cs"]).into_iter().map(|c| c.to_digit(radix)).collect::<Vec<_>>()
        ),
        "is_digit" => json!(
            chars_of(&request["cs"]).into_iter().map(|c| c.is_digit(radix)).collect::<Vec<_>>()
        ),
        "from_digit" => json!(
            request["ns"]
                .as_array()
                .unwrap()
                .iter()
                .map(|n| char::from_digit(n.as_u64().unwrap() as u32, radix).map(|c| c as u32))
                .collect::<Vec<_>>()
        ),
        "from_u32" => json!(
            request["ns"]
                .as_array()
                .unwrap()
                .iter()
                .map(|n| char::from_u32(n.as_u64().unwrap() as u32).map(|c| c as u32))
                .collect::<Vec<_>>()
        ),
        _ => crate::unknown(request),
    }
}

/// Every pattern-free `str` method on one string.
fn str_all(request: &Value) -> Value {
    let s = crate::text(request, "s");
    let char_indices: Vec<Value> = s.char_indices().map(|(i, c)| json!([i, c as u32])).collect();
    let boundaries: Vec<Value> = (0..=s.len() + 2)
        .map(|i| json!([s.is_char_boundary(i), s.floor_char_boundary(i), s.ceil_char_boundary(i)]))
        .collect();
    let mut lower_in_place = s.to_owned();
    lower_in_place.make_ascii_lowercase();
    let mut upper_in_place = s.to_owned();
    upper_in_place.make_ascii_uppercase();
    json!({
        "chars": codes(s.chars()),
        "chars_rev": codes(s.chars().rev()),
        "chars_count": s.chars().count(),
        "chars_next": s.chars().next().map(|c| c as u32),
        "chars_next_back": s.chars().next_back().map(|c| c as u32),
        "char_indices": char_indices,
        "boundaries": boundaries,
        "trim": s.trim(),
        "trim_start": s.trim_start(),
        "trim_end": s.trim_end(),
        "trim_ascii": s.trim_ascii(),
        "trim_ascii_start": s.trim_ascii_start(),
        "trim_ascii_end": s.trim_ascii_end(),
        "lines": s.lines().collect::<Vec<_>>(),
        "split_whitespace": s.split_whitespace().collect::<Vec<_>>(),
        "split_ascii_whitespace": s.split_ascii_whitespace().collect::<Vec<_>>(),
        "to_lowercase": s.to_lowercase(),
        "to_uppercase": s.to_uppercase(),
        "to_ascii_lowercase": s.to_ascii_lowercase(),
        "to_ascii_uppercase": s.to_ascii_uppercase(),
        "make_ascii_lowercase": lower_in_place,
        "make_ascii_uppercase": upper_in_place,
        "is_ascii": s.is_ascii(),
        "encode_utf16": s.encode_utf16().collect::<Vec<_>>(),
        "escape_debug": s.escape_debug().to_string(),
        "escape_default": s.escape_default().to_string(),
        "escape_unicode": s.escape_unicode().to_string(),
        "debug": format!("{s:?}"),
    })
}

fn index(request: &Value) -> Value {
    let s = crate::text(request, "s");
    let i = num(request, "i");
    json!([s.is_char_boundary(i), s.floor_char_boundary(i), s.ceil_char_boundary(i)])
}

/// Indexing (which panics) or `get` (which does not), by range kind.
fn slice(request: &Value) -> Value {
    let s = crate::text(request, "s");
    let (a, b) = (num(request, "a"), num(request, "b"));
    match crate::text(request, "f") {
        "range" => json!(&s[a..b]),
        "from" => json!(&s[a..]),
        "to" => json!(&s[..b]),
        "inclusive" => json!(&s[a..=b]),
        "to_inclusive" => json!(&s[..=b]),
        "get_range" => json!(s.get(a..b)),
        "get_from" => json!(s.get(a..)),
        "get_to" => json!(s.get(..b)),
        "get_inclusive" => json!(s.get(a..=b)),
        "get_to_inclusive" => json!(s.get(..=b)),
        _ => crate::unknown(request),
    }
}

fn split_at(request: &Value) -> Value {
    let s = crate::text(request, "s");
    let mid = num(request, "mid");
    match crate::text(request, "f") {
        "split_at" => {
            let (head, tail) = s.split_at(mid);
            json!([head, tail])
        }
        "split_at_checked" => json!(s.split_at_checked(mid).map(|(head, tail)| json!([head, tail]))),
        _ => crate::unknown(request),
    }
}

fn predicate(name: &str) -> fn(char) -> bool {
    match name {
        "is_whitespace" => char::is_whitespace,
        "is_alphabetic" => char::is_alphabetic,
        "is_numeric" => char::is_numeric,
        "is_alphanumeric" => char::is_alphanumeric,
        "is_uppercase" => char::is_uppercase,
        "is_lowercase" => char::is_lowercase,
        "is_control" => char::is_control,
        "is_ascii_digit" => |c| c.is_ascii_digit(),
        "is_ascii_punctuation" => |c| c.is_ascii_punctuation(),
        "is_ascii" => |c| c.is_ascii(),
        "not_ascii" => |c| !c.is_ascii(),
        "always" => |_| true,
        "never" => |_| false,
        other => panic!("unknown predicate {other}"),
    }
}

fn pairs(items: impl Iterator<Item = (usize, impl AsRef<str>)>) -> Vec<Value> {
    items.map(|(at, part)| json!([at, part.as_ref()])).collect()
}

fn halves(found: Option<(&str, &str)>) -> Value {
    json!(found.map(|(head, tail)| json!([head, tail])))
}

/// Every method that takes a pattern, for one haystack and one pattern. `$p`
/// is evaluated once for each method, so it must be `Copy`.
macro_rules! pattern_reply {
    ($s:expr, $p:expr, $n:expr, $to:expr) => {{
        let s: &str = $s;
        let n: usize = $n;
        let to: &str = $to;
        let mut reply = Map::new();
        reply.insert("contains".into(), json!(s.contains($p)));
        reply.insert("starts_with".into(), json!(s.starts_with($p)));
        reply.insert("ends_with".into(), json!(s.ends_with($p)));
        reply.insert("find".into(), json!(s.find($p)));
        reply.insert("rfind".into(), json!(s.rfind($p)));
        reply.insert("split".into(), json!(s.split($p).collect::<Vec<_>>()));
        reply.insert("split_inclusive".into(), json!(s.split_inclusive($p).collect::<Vec<_>>()));
        reply.insert("rsplit".into(), json!(s.rsplit($p).collect::<Vec<_>>()));
        reply.insert("split_terminator".into(), json!(s.split_terminator($p).collect::<Vec<_>>()));
        reply
            .insert("rsplit_terminator".into(), json!(s.rsplit_terminator($p).collect::<Vec<_>>()));
        reply.insert("splitn".into(), json!(s.splitn(n, $p).collect::<Vec<_>>()));
        reply.insert("rsplitn".into(), json!(s.rsplitn(n, $p).collect::<Vec<_>>()));
        reply.insert("split_once".into(), halves(s.split_once($p)));
        reply.insert("rsplit_once".into(), halves(s.rsplit_once($p)));
        reply.insert("matches".into(), json!(s.matches($p).collect::<Vec<_>>()));
        reply.insert("rmatches".into(), json!(s.rmatches($p).collect::<Vec<_>>()));
        reply.insert("match_indices".into(), json!(pairs(s.match_indices($p))));
        reply.insert("rmatch_indices".into(), json!(pairs(s.rmatch_indices($p))));
        reply.insert("trim_start_matches".into(), json!(s.trim_start_matches($p)));
        reply.insert("trim_end_matches".into(), json!(s.trim_end_matches($p)));
        reply.insert("strip_prefix".into(), json!(s.strip_prefix($p)));
        reply.insert("strip_suffix".into(), json!(s.strip_suffix($p)));
        reply.insert("replace".into(), json!(s.replace($p, to)));
        reply.insert("replacen".into(), json!(s.replacen($p, to, n)));
        reply
    }};
}

fn pattern(request: &Value) -> Value {
    let s = crate::text(request, "s");
    let n = num(request, "n");
    let to = crate::text(request, "to");
    let pat = &request["pat"];
    let reply = if let Some(needle) = pat.get("s").and_then(Value::as_str) {
        // `&str` has no double-ended searcher, so no `trim_matches`.
        pattern_reply!(s, needle, n, to)
    } else if let Some(c) = pat.get("c") {
        let c = chr(c);
        let mut reply = pattern_reply!(s, c, n, to);
        reply.insert("trim_matches".into(), json!(s.trim_matches(c)));
        reply
    } else if let Some(set) = pat.get("cs") {
        let set = chars_of(set);
        let set = set.as_slice();
        let mut reply = pattern_reply!(s, set, n, to);
        reply.insert("trim_matches".into(), json!(s.trim_matches(set)));
        reply
    } else if let Some(name) = pat.get("f").and_then(Value::as_str) {
        let f = predicate(name);
        let mut reply = pattern_reply!(s, f, n, to);
        reply.insert("trim_matches".into(), json!(s.trim_matches(f)));
        reply
    } else {
        return crate::unknown(request);
    };
    Value::Object(reply)
}

/// `String` methods that change the string in place: the reply is the string
/// afterwards and what the method returned.
fn string_mut(request: &Value) -> Value {
    let mut s = crate::text(request, "s").to_owned();
    let (a, b) = (num(request, "a"), num(request, "b"));
    let with = crate::text(request, "with");
    let ret = match crate::text(request, "f") {
        "truncate" => {
            s.truncate(a);
            Value::Null
        }
        "pop" => json!(s.pop().map(|c| c as u32)),
        "remove" => json!(s.remove(a) as u32),
        "insert" => {
            s.insert(a, chr(&request["c"]));
            Value::Null
        }
        "insert_str" => {
            s.insert_str(a, with);
            Value::Null
        }
        "drain" => json!(s.drain(a..b).collect::<String>()),
        "drain_inclusive" => json!(s.drain(a..=b).collect::<String>()),
        "retain" => {
            let keep = predicate(crate::text(request, "keep"));
            s.retain(keep);
            Value::Null
        }
        "split_off" => json!(s.split_off(a)),
        "replace_range" => {
            s.replace_range(a..b, with);
            Value::Null
        }
        "replace_range_inclusive" => {
            s.replace_range(a..=b, with);
            Value::Null
        }
        _ => return crate::unknown(request),
    };
    json!({ "s": s, "ret": ret })
}

fn int<T: ToString, E: Display + Debug>(parsed: Result<T, E>) -> Value {
    result(parsed.map(|value| value.to_string()))
}

/// `s.parse::<T>()`. `"t"` picks the group of types, which keeps replies
/// small: `"ints"`, `"floats"` (the reply is the float's bits in hex) or
/// `"misc"` (bool and char).
fn parse(request: &Value) -> Value {
    let s = crate::text(request, "s");
    match crate::text(request, "t") {
        "ints" => json!({
            "u8": int(s.parse::<u8>()),
            "u16": int(s.parse::<u16>()),
            "u32": int(s.parse::<u32>()),
            "u64": int(s.parse::<u64>()),
            "u128": int(s.parse::<u128>()),
            "usize": int(s.parse::<usize>()),
            "i8": int(s.parse::<i8>()),
            "i16": int(s.parse::<i16>()),
            "i32": int(s.parse::<i32>()),
            "i64": int(s.parse::<i64>()),
            "i128": int(s.parse::<i128>()),
            "isize": int(s.parse::<isize>()),
        }),
        "floats" => json!({
            "f32": result(s.parse::<f32>().map(|value| format!("{:08x}", value.to_bits()))),
            "f64": result(s.parse::<f64>().map(|value| format!("{:016x}", value.to_bits()))),
        }),
        "misc" => json!({
            "bool": result(s.parse::<bool>()),
            "char": result(s.parse::<char>().map(|c| c as u32)),
        }),
        _ => crate::unknown(request),
    }
}

fn radix(request: &Value) -> Value {
    let s = crate::text(request, "s");
    let radix = num(request, "radix") as u32;
    json!({
        "u8": int(u8::from_str_radix(s, radix)),
        "u16": int(u16::from_str_radix(s, radix)),
        "u32": int(u32::from_str_radix(s, radix)),
        "u64": int(u64::from_str_radix(s, radix)),
        "u128": int(u128::from_str_radix(s, radix)),
        "usize": int(usize::from_str_radix(s, radix)),
        "i8": int(i8::from_str_radix(s, radix)),
        "i16": int(i16::from_str_radix(s, radix)),
        "i32": int(i32::from_str_radix(s, radix)),
        "i64": int(i64::from_str_radix(s, radix)),
        "i128": int(i128::from_str_radix(s, radix)),
        "isize": int(isize::from_str_radix(s, radix)),
    })
}

fn pair(request: &Value) -> Value {
    let (a, b) = (crate::text(request, "a"), crate::text(request, "b"));
    json!({
        "eq_ignore_ascii_case": a.eq_ignore_ascii_case(b),
        "cmp": a.cmp(b) as i8,
    })
}

/// The ASCII methods of `[u8]`.
fn bytes_ops(request: &Value) -> Value {
    let a = bytes_of(request, "a");
    let b = bytes_of(request, "b");
    let classes: Vec<u32> = a
        .iter()
        .map(|byte| {
            let flags = [
                byte.is_ascii(),
                byte.is_ascii_alphabetic(),
                byte.is_ascii_digit(),
                byte.is_ascii_alphanumeric(),
                byte.is_ascii_hexdigit(),
                byte.is_ascii_punctuation(),
                byte.is_ascii_graphic(),
                byte.is_ascii_whitespace(),
                byte.is_ascii_control(),
                byte.is_ascii_uppercase(),
                byte.is_ascii_lowercase(),
            ];
            flags.iter().enumerate().fold(0, |mask, (bit, &set)| mask | (u32::from(set) << bit))
        })
        .collect();
    json!({
        "is_ascii": a.is_ascii(),
        "trim_ascii": hex(a.trim_ascii()),
        "trim_ascii_start": hex(a.trim_ascii_start()),
        "trim_ascii_end": hex(a.trim_ascii_end()),
        "to_ascii_lowercase": hex(&a.to_ascii_lowercase()),
        "to_ascii_uppercase": hex(&a.to_ascii_uppercase()),
        "eq_ignore_ascii_case": a.eq_ignore_ascii_case(&b),
        "classes": classes,
        "lower": hex(&a.iter().map(u8::to_ascii_lowercase).collect::<Vec<_>>()),
        "upper": hex(&a.iter().map(u8::to_ascii_uppercase).collect::<Vec<_>>()),
    })
}

/// One error value, formatted with `{}` and `{:?}` under a few sets of flags:
/// whether the `Display` impl pads (`f.pad`) or writes (`write!`), and how the
/// derived `Debug` passes the flags on to the fields.
fn formatted<E: Display + Debug>(error: E) -> Value {
    json!({
        "display": format!("{error}"),
        "width": format!("{error:>60}"),
        "fill": format!("{error:*^70}"),
        "precision": format!("{error:.7}"),
        "width_precision": format!("{error:<12.4}|"),
        "debug": format!("{error:?}"),
        "pretty": format!("{error:#?}"),
        "debug_width": format!("{error:>4?}"),
    })
}

/// The errors of this module's conversions and parsers, made from `"s"`, `"b"`
/// or `"u"` as the request's `"kind"` says, then formatted.
fn fmt(request: &Value) -> Value {
    let s = crate::text(request, "s");
    match crate::text(request, "kind") {
        "utf8" => formatted(std::str::from_utf8(&bytes_of(request, "b")).unwrap_err()),
        "from_utf8" => formatted(String::from_utf8(bytes_of(request, "b")).unwrap_err()),
        "from_utf16" => formatted(String::from_utf16(&units_of(request)).unwrap_err()),
        "int" => formatted(s.parse::<i8>().unwrap_err()),
        "float" => formatted(s.parse::<f64>().unwrap_err()),
        "bool" => formatted(s.parse::<bool>().unwrap_err()),
        "char" => formatted(s.parse::<char>().unwrap_err()),
        _ => crate::unknown(request),
    }
}
