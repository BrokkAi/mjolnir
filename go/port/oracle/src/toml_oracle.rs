//! Oracle module `toml`: the crates toml 0.8.2 and toml_edit 0.20.2. Requests
//! are objects with an `"op"` member; the Go tests in `go/crates/toml` and
//! `go/crates/tomledit` say what each operation is for.
//!
//! A `toml::Value` crosses the boundary in a form that keeps its type: one
//! object with a single member, `{"s": string}`, `{"i": "decimal"}`,
//! `{"f": "bits in hex"}`, `{"b": bool}`, `{"d": "datetime"}`, `{"a": [..]}`
//! or `{"t": {..}}`.

use std::str::FromStr;

use serde_json::{Map, Value, json};
use toml_edit::{Document, Item, TableLike};

pub fn handle(request: &Value) -> Value {
    let op = crate::text(request, "op");
    match op {
        // toml_edit: parse a document and print it back.
        "edit_parse" => edit_parse(crate::text(request, "text"), false),
        // toml_edit: parse a document and describe everything the parser kept.
        "edit_dump" => edit_parse(crate::text(request, "text"), true),
        // toml_edit: parse a key, a dotted key or a value on its own.
        "edit_key" => match toml_edit::Key::from_str(crate::text(request, "text")) {
            Ok(key) => json!({"ok": {"key": key.get(), "display": key.to_string()}}),
            Err(error) => json!({"err": error.to_string()}),
        },
        "edit_key_path" => match toml_edit::Key::parse(crate::text(request, "text")) {
            Ok(keys) => json!({"ok": keys.iter().map(dump_key).collect::<Vec<_>>()}),
            Err(error) => json!({"err": error.to_string()}),
        },
        "edit_value" => match toml_edit::Value::from_str(crate::text(request, "text")) {
            Ok(value) => json!({"ok": {"display": value.to_string(), "dump": dump_value(&value)}}),
            Err(error) => json!({"err": error.to_string()}),
        },
        // toml_edit: apply an edit script to a document.
        "edit_script" => edit_script(request),
        // Mjolnir's own save path (mj-core/src/config/document.rs).
        "edit_in_place" => {
            let existing = crate::text(request, "existing");
            let loaded = crate::text(request, "loaded");
            let updated = crate::text(request, "updated");
            match edit_in_place(existing, loaded, updated) {
                Some(text) => json!({"ok": text}),
                None => json!({"none": true}),
            }
        }
        // toml: parse into a Value.
        "parse" => match toml::from_str::<toml::Value>(crate::text(request, "text")) {
            Ok(value) => json!({"ok": to_canonical(&value)}),
            Err(error) => de_error(&error),
        },
        "parse_table" => match crate::text(request, "text").parse::<toml::Table>() {
            Ok(table) => json!({"ok": to_canonical(&toml::Value::Table(table))}),
            Err(error) => de_error(&error),
        },
        // toml: serialize a Value.
        "to_string" | "to_string_pretty" | "display" | "table_display" | "try_from" => {
            let value = match from_canonical(&request["value"]) {
                Ok(value) => value,
                Err(message) => return json!({"oracle_error": message}),
            };
            match op {
                "to_string" => ser_result(toml::to_string(&value)),
                "to_string_pretty" => ser_result(toml::to_string_pretty(&value)),
                "display" => json!({"ok": value.to_string()}),
                "table_display" => match value {
                    toml::Value::Table(table) => json!({"ok": table.to_string()}),
                    _ => json!({"oracle_error": "table_display needs a table"}),
                },
                _ => match toml::Value::try_from(&value) {
                    Ok(value) => json!({"ok": to_canonical(&value)}),
                    Err(error) => json!({"err": error.to_string()}),
                },
            }
        }
        // toml with serde-derived types: read a document into the type named
        // by "type" and write it back out every way toml writes.
        "derive" => derived::run(crate::text(request, "type"), crate::text(request, "text"))
            .unwrap_or_else(|| crate::unknown(request)),
        "datetime" => match toml::value::Datetime::from_str(crate::text(request, "text")) {
            Ok(datetime) => json!({"ok": datetime.to_string()}),
            Err(error) => json!({"err": error.to_string()}),
        },
        // toml_edit: how a value made from scratch is written.
        "edit_repr" => {
            let value = match &request["value"] {
                Value::Object(map) if map.contains_key("s") => {
                    toml_edit::Value::from(map["s"].as_str().unwrap_or_default())
                }
                Value::Object(map) if map.contains_key("f") => {
                    toml_edit::Value::from(f64::from_bits(
                        u64::from_str_radix(map["f"].as_str().unwrap_or_default(), 16).unwrap_or_default(),
                    ))
                }
                Value::Object(map) if map.contains_key("i") => toml_edit::Value::from(
                    map["i"].as_str().unwrap_or_default().parse::<i64>().unwrap_or_default(),
                ),
                _ => return crate::unknown(request),
            };
            let key = toml_edit::Key::new(crate::text(request, "key"));
            json!({"value": value.to_string(), "key": key.to_string()})
        }
        _ => crate::unknown(request),
    }
}

fn de_error(error: &toml::de::Error) -> Value {
    json!({
        "err": error.to_string(),
        "message": error.message(),
        "span": error.span().map(|span| vec![span.start, span.end]),
    })
}

fn ser_result(result: Result<String, toml::ser::Error>) -> Value {
    match result {
        Ok(text) => json!({"ok": text}),
        Err(error) => json!({"err": error.to_string()}),
    }
}

fn to_canonical(value: &toml::Value) -> Value {
    match value {
        toml::Value::String(s) => json!({"s": s}),
        toml::Value::Integer(i) => json!({"i": i.to_string()}),
        toml::Value::Float(f) => json!({"f": format!("{:016x}", f.to_bits())}),
        toml::Value::Boolean(b) => json!({"b": b}),
        toml::Value::Datetime(d) => json!({"d": d.to_string()}),
        toml::Value::Array(a) => json!({"a": a.iter().map(to_canonical).collect::<Vec<_>>()}),
        toml::Value::Table(t) => {
            let mut map = Map::new();
            for (key, value) in t {
                map.insert(key.clone(), to_canonical(value));
            }
            json!({"t": map})
        }
    }
}

fn from_canonical(value: &Value) -> Result<toml::Value, String> {
    let Some(map) = value.as_object() else {
        return Err(format!("not a canonical value: {value}"));
    };
    let Some((kind, inner)) = map.iter().next() else {
        return Err(format!("not a canonical value: {value}"));
    };
    let text = || inner.as_str().ok_or_else(|| format!("not a canonical value: {value}"));
    Ok(match kind.as_str() {
        "s" => toml::Value::String(text()?.to_owned()),
        "i" => toml::Value::Integer(text()?.parse().map_err(|_| format!("bad integer in {value}"))?),
        "f" => toml::Value::Float(f64::from_bits(
            u64::from_str_radix(text()?, 16).map_err(|_| format!("bad float in {value}"))?,
        )),
        "b" => toml::Value::Boolean(inner.as_bool().ok_or_else(|| format!("bad bool in {value}"))?),
        "d" => toml::Value::Datetime(text()?.parse().map_err(|_| format!("bad datetime in {value}"))?),
        "a" => toml::Value::Array(
            inner
                .as_array()
                .ok_or_else(|| format!("bad array in {value}"))?
                .iter()
                .map(from_canonical)
                .collect::<Result<_, _>>()?,
        ),
        "t" => {
            let mut table = toml::Table::new();
            for (key, value) in inner.as_object().ok_or_else(|| format!("bad table in {value}"))? {
                table.insert(key.clone(), from_canonical(value)?);
            }
            toml::Value::Table(table)
        }
        _ => return Err(format!("not a canonical value: {value}")),
    })
}

// toml_edit

fn edit_parse(text: &str, dump: bool) -> Value {
    match text.parse::<Document>() {
        Ok(document) if dump => json!({
            "ok": document.to_string(),
            "dump": dump_table(document.as_table()),
            "trailing": document.trailing().as_str(),
        }),
        Ok(document) => json!({"ok": document.to_string()}),
        Err(error) => json!({
            "err": error.to_string(),
            "message": error.message(),
            "span": error.span().map(|span| vec![span.start, span.end]),
        }),
    }
}

fn dump_decor(decor: &toml_edit::Decor) -> Value {
    json!([
        decor.prefix().map(|raw| raw.as_str()),
        decor.suffix().map(|raw| raw.as_str()),
    ])
}

fn dump_key(key: &toml_edit::Key) -> Value {
    json!({
        "key": key.get(),
        "repr": key.as_repr().map(|repr| repr.as_raw().as_str()),
        "decor": dump_decor(key.decor()),
    })
}

fn dump_value(value: &toml_edit::Value) -> Value {
    fn repr(repr: Option<&toml_edit::Repr>) -> Option<Option<&str>> {
        repr.map(|repr| repr.as_raw().as_str())
    }
    match value {
        toml_edit::Value::String(f) => json!({
            "kind": "string", "value": f.value(), "repr": repr(f.as_repr()), "decor": dump_decor(f.decor()),
        }),
        toml_edit::Value::Integer(f) => json!({
            "kind": "integer", "value": f.value().to_string(), "repr": repr(f.as_repr()), "decor": dump_decor(f.decor()),
        }),
        toml_edit::Value::Float(f) => json!({
            "kind": "float", "value": format!("{:016x}", f.value().to_bits()), "repr": repr(f.as_repr()),
            "decor": dump_decor(f.decor()),
        }),
        toml_edit::Value::Boolean(f) => json!({
            "kind": "boolean", "value": f.value(), "repr": repr(f.as_repr()), "decor": dump_decor(f.decor()),
        }),
        toml_edit::Value::Datetime(f) => json!({
            "kind": "datetime", "value": f.value().to_string(), "repr": repr(f.as_repr()),
            "decor": dump_decor(f.decor()),
        }),
        toml_edit::Value::Array(array) => json!({
            "kind": "array",
            "decor": dump_decor(array.decor()),
            "trailing": array.trailing().as_str(),
            "trailing_comma": array.trailing_comma(),
            "values": array.iter().map(dump_value).collect::<Vec<_>>(),
        }),
        toml_edit::Value::InlineTable(table) => json!({
            "kind": "inline table",
            "decor": dump_decor(table.decor()),
            "preamble": table.preamble().as_str(),
            "dotted": table.is_dotted(),
            "items": table
                .iter()
                .map(|(key, value)| {
                    let (full, _) = table.get_key_value(key).expect("key from iter");
                    json!([dump_key(full), dump_value(value)])
                })
                .collect::<Vec<_>>(),
        }),
    }
}

fn dump_item(item: &Item) -> Value {
    match item {
        Item::None => json!({"kind": "none"}),
        Item::Value(value) => dump_value(value),
        Item::Table(table) => dump_table(table),
        Item::ArrayOfTables(array) => json!({
            "kind": "array of tables",
            "tables": array.iter().map(dump_table).collect::<Vec<_>>(),
        }),
    }
}

fn dump_table(table: &toml_edit::Table) -> Value {
    json!({
        "kind": "table",
        "decor": dump_decor(table.decor()),
        "implicit": table.is_implicit(),
        "dotted": table.is_dotted(),
        "position": table.position(),
        "items": table
            .iter()
            .map(|(key, item)| {
                let (full, _) = table.get_key_value(key).expect("key from iter");
                json!([dump_key(full), dump_item(item)])
            })
            .collect::<Vec<_>>(),
    })
}

/// A scalar or array in an edit script: `{"s": ..}`, `{"i": ..}`, `{"f": ..}`,
/// `{"b": ..}`, `{"d": ..}` or `{"a": [..]}`.
fn edit_value(value: &Value) -> Result<toml_edit::Value, String> {
    let map = value.as_object().ok_or_else(|| format!("not an edit value: {value}"))?;
    let (kind, inner) = map.iter().next().ok_or_else(|| format!("not an edit value: {value}"))?;
    let text = || inner.as_str().ok_or_else(|| format!("not an edit value: {value}"));
    Ok(match kind.as_str() {
        "s" => toml_edit::Value::from(text()?),
        "i" => toml_edit::Value::from(text()?.parse::<i64>().map_err(|e| e.to_string())?),
        "f" => toml_edit::Value::from(f64::from_bits(
            u64::from_str_radix(text()?, 16).map_err(|e| e.to_string())?,
        )),
        "b" => toml_edit::Value::from(inner.as_bool().ok_or("bad bool")?),
        "d" => toml_edit::Value::from(text()?.parse::<toml_edit::Datetime>().map_err(|e| e.to_string())?),
        "a" => {
            let mut array = toml_edit::Array::new();
            for element in inner.as_array().ok_or("bad array")? {
                array.push(edit_value(element)?);
            }
            toml_edit::Value::Array(array)
        }
        "parse" => text()?.parse::<toml_edit::Value>().map_err(|e| e.to_string())?,
        _ => return Err(format!("not an edit value: {value}")),
    })
}

fn path_of(value: &Value) -> Vec<PathStep> {
    value
        .as_array()
        .map(|steps| {
            steps
                .iter()
                .map(|step| match step {
                    Value::String(key) => PathStep::Key(key.clone()),
                    other => PathStep::Index(other.as_u64().unwrap_or_default() as usize),
                })
                .collect()
        })
        .unwrap_or_default()
}

enum PathStep {
    Key(String),
    Index(usize),
}

fn item_at<'a>(document: &'a mut Document, path: &[PathStep]) -> Option<&'a mut Item> {
    let mut item = document.as_item_mut();
    for step in path {
        item = match step {
            PathStep::Key(key) => item.get_mut(key.as_str())?,
            PathStep::Index(index) => item.get_mut(*index)?,
        };
    }
    Some(item)
}

fn edit_script(request: &Value) -> Value {
    let mut document = match crate::text(request, "text").parse::<Document>() {
        Ok(document) => document,
        Err(error) => return json!({"err": error.to_string()}),
    };
    let mut log = Vec::new();
    for step in request["steps"].as_array().map(Vec::as_slice).unwrap_or_default() {
        let step_op = step[0].as_str().unwrap_or_default();
        let path = path_of(&step[1]);
        let outcome = edit_step(&mut document, step_op, &path, step);
        match outcome {
            Ok(note) => log.push(note),
            Err(message) => return json!({"oracle_error": format!("step {step}: {message}")}),
        }
    }
    json!({"ok": document.to_string(), "log": log})
}

fn edit_step(document: &mut Document, op: &str, path: &[PathStep], step: &Value) -> Result<Value, String> {
    match op {
        // document[k1][k2].. = value(v), creating what is missing as IndexMut does.
        "index_set" => {
            let value = edit_value(&step[2])?;
            let mut item = document.as_item_mut();
            for part in path {
                item = match part {
                    PathStep::Key(key) => match toml_edit::Item::get_mut(item, key.as_str()) {
                        Some(_) => &mut item[key.as_str()],
                        None => return Ok(json!("index not found")),
                    },
                    PathStep::Index(index) => match item.get_mut(*index) {
                        Some(found) => found,
                        None => return Ok(json!("index not found")),
                    },
                };
            }
            *item = toml_edit::value(value);
            Ok(json!("set"))
        }
        // Replace the value of an item, keeping its decor.
        "replace_keep_decor" => {
            let value = edit_value(&step[2])?;
            let Some(item) = item_at(document, path) else { return Ok(json!("missing")) };
            let Some(existing) = item.as_value_mut() else { return Ok(json!("not a value")) };
            let decor = existing.decor().clone();
            let mut value = value;
            *value.decor_mut() = decor;
            *existing = value;
            Ok(json!("replaced"))
        }
        // Replace an item with a fresh value, with default decor.
        "replace" => {
            let value = edit_value(&step[2])?;
            let Some(item) = item_at(document, path) else { return Ok(json!("missing")) };
            *item = Item::Value(value);
            Ok(json!("replaced"))
        }
        // TableLike::insert of an item taken from another document.
        "insert_from" => {
            let key = step[2].as_str().unwrap_or_default();
            let source = step[3].as_str().unwrap_or_default().parse::<Document>().map_err(|e| e.to_string())?;
            let source_path = path_of(&step[4]);
            let mut source_item = source.as_item();
            for part in &source_path {
                source_item = match part {
                    PathStep::Key(key) => source_item.get(key.as_str()).ok_or("no such source item")?,
                    PathStep::Index(index) => source_item.get(*index).ok_or("no such source item")?,
                };
            }
            let mut inserted = source_item.clone();
            if let Some(position) = step[5].as_u64() {
                let mut next = position as usize;
                place(&mut inserted, &mut next);
            }
            let Some(item) = item_at(document, path) else { return Ok(json!("missing")) };
            let Some(table) = item.as_table_like_mut() else { return Ok(json!("not a table")) };
            let previous = table.insert(key, inserted);
            Ok(json!(previous.is_some()))
        }
        "insert" => {
            let key = step[2].as_str().unwrap_or_default();
            let value = edit_value(&step[3])?;
            let Some(item) = item_at(document, path) else { return Ok(json!("missing")) };
            let Some(table) = item.as_table_like_mut() else { return Ok(json!("not a table")) };
            let previous = table.insert(key, toml_edit::value(value));
            Ok(json!(previous.is_some()))
        }
        "insert_table" => {
            let key = step[2].as_str().unwrap_or_default();
            let Some(item) = item_at(document, path) else { return Ok(json!("missing")) };
            let Some(table) = item.as_table_mut() else { return Ok(json!("not a table")) };
            let mut new_table = toml_edit::Table::new();
            if let Some(position) = step[3].as_u64() {
                new_table.set_position(position as usize);
            }
            if let Some(implicit) = step[4].as_bool() {
                new_table.set_implicit(implicit);
            }
            let previous = table.insert(key, Item::Table(new_table));
            Ok(json!(previous.is_some()))
        }
        "remove" => {
            let key = step[2].as_str().unwrap_or_default();
            let Some(item) = item_at(document, path) else { return Ok(json!("missing")) };
            let Some(table) = item.as_table_like_mut() else { return Ok(json!("not a table")) };
            let removed = table.remove(key);
            Ok(json!(removed.map(|item| item.type_name())))
        }
        "keys" => {
            let Some(item) = item_at(document, path) else { return Ok(json!("missing")) };
            let Some(table) = item.as_table_like() else { return Ok(json!("not a table")) };
            Ok(json!(table.iter().map(|(key, item)| json!([key, item.type_name()])).collect::<Vec<_>>()))
        }
        "get" => {
            let Some(item) = item_at(document, path) else { return Ok(json!("missing")) };
            Ok(json!({
                "type": item.type_name(),
                "str": item.as_str(),
                "integer": item.as_integer(),
                "bool": item.as_bool(),
                "display": item.to_string(),
            }))
        }
        "set_position" => {
            let Some(item) = item_at(document, path) else { return Ok(json!("missing")) };
            let Some(table) = item.as_table_mut() else { return Ok(json!("not a table")) };
            table.set_position(step[2].as_u64().unwrap_or_default() as usize);
            Ok(json!("positioned"))
        }
        "set_implicit" => {
            let Some(item) = item_at(document, path) else { return Ok(json!("missing")) };
            let Some(table) = item.as_table_mut() else { return Ok(json!("not a table")) };
            table.set_implicit(step[2].as_bool().unwrap_or_default());
            Ok(json!("done"))
        }
        "set_dotted" => {
            let Some(item) = item_at(document, path) else { return Ok(json!("missing")) };
            let Some(table) = item.as_table_like_mut() else { return Ok(json!("not a table")) };
            table.set_dotted(step[2].as_bool().unwrap_or_default());
            Ok(json!("done"))
        }
        "fmt" => {
            let Some(item) = item_at(document, path) else { return Ok(json!("missing")) };
            match item {
                Item::Table(table) => table.fmt(),
                Item::Value(toml_edit::Value::InlineTable(table)) => table.fmt(),
                Item::Value(toml_edit::Value::Array(array)) => array.fmt(),
                _ => return Ok(json!("not formattable")),
            }
            Ok(json!("formatted"))
        }
        "sort_values" => {
            let Some(item) = item_at(document, path) else { return Ok(json!("missing")) };
            let Some(table) = item.as_table_like_mut() else { return Ok(json!("not a table")) };
            table.sort_values();
            Ok(json!("sorted"))
        }
        "decor" => {
            let Some(item) = item_at(document, path) else { return Ok(json!("missing")) };
            let decor = match item {
                Item::Value(value) => value.decor_mut(),
                Item::Table(table) => table.decor_mut(),
                _ => return Ok(json!("no decor")),
            };
            match (step[2].as_str(), step[3].as_str()) {
                (Some(prefix), Some(suffix)) => *decor = toml_edit::Decor::new(prefix, suffix),
                (Some(prefix), None) => decor.set_prefix(prefix),
                (None, Some(suffix)) => decor.set_suffix(suffix),
                (None, None) => decor.clear(),
            }
            Ok(json!("decorated"))
        }
        "key_decor" => {
            let key = step[2].as_str().unwrap_or_default();
            let Some(item) = item_at(document, path) else { return Ok(json!("missing")) };
            let Some(table) = item.as_table_like_mut() else { return Ok(json!("not a table")) };
            let Some(decor) = table.key_decor_mut(key) else { return Ok(json!("no key")) };
            match (step[3].as_str(), step[4].as_str()) {
                (Some(prefix), Some(suffix)) => *decor = toml_edit::Decor::new(prefix, suffix),
                (Some(prefix), None) => decor.set_prefix(prefix),
                (None, Some(suffix)) => decor.set_suffix(suffix),
                (None, None) => decor.clear(),
            }
            Ok(json!("decorated"))
        }
        "array_push" => {
            let value = edit_value(&step[2])?;
            let Some(item) = item_at(document, path) else { return Ok(json!("missing")) };
            let Some(array) = item.as_array_mut() else { return Ok(json!("not an array")) };
            array.push(value);
            Ok(json!(array.len()))
        }
        "array_push_formatted" => {
            let value = edit_value(&step[2])?;
            let Some(item) = item_at(document, path) else { return Ok(json!("missing")) };
            let Some(array) = item.as_array_mut() else { return Ok(json!("not an array")) };
            array.push_formatted(value);
            Ok(json!(array.len()))
        }
        "array_insert" => {
            let index = step[2].as_u64().unwrap_or_default() as usize;
            let value = edit_value(&step[3])?;
            let Some(item) = item_at(document, path) else { return Ok(json!("missing")) };
            let Some(array) = item.as_array_mut() else { return Ok(json!("not an array")) };
            if index > array.len() {
                return Ok(json!("out of bounds"));
            }
            array.insert(index, value);
            Ok(json!(array.len()))
        }
        "array_replace" => {
            let index = step[2].as_u64().unwrap_or_default() as usize;
            let value = edit_value(&step[3])?;
            let Some(item) = item_at(document, path) else { return Ok(json!("missing")) };
            let Some(array) = item.as_array_mut() else { return Ok(json!("not an array")) };
            if index >= array.len() {
                return Ok(json!("out of bounds"));
            }
            let old = array.replace(index, value);
            Ok(json!(old.to_string()))
        }
        "array_remove" => {
            let index = step[2].as_u64().unwrap_or_default() as usize;
            let Some(item) = item_at(document, path) else { return Ok(json!("missing")) };
            let Some(array) = item.as_array_mut() else { return Ok(json!("not an array")) };
            if index >= array.len() {
                return Ok(json!("out of bounds"));
            }
            let old = array.remove(index);
            Ok(json!(old.to_string()))
        }
        "array_trailing" => {
            let Some(item) = item_at(document, path) else { return Ok(json!("missing")) };
            let Some(array) = item.as_array_mut() else { return Ok(json!("not an array")) };
            if let Some(trailing) = step[2].as_str() {
                array.set_trailing(trailing);
            }
            if let Some(comma) = step[3].as_bool() {
                array.set_trailing_comma(comma);
            }
            Ok(json!("done"))
        }
        "aot_push" => {
            let Some(item) = item_at(document, path) else { return Ok(json!("missing")) };
            let Some(array) = item.as_array_of_tables_mut() else { return Ok(json!("not an array of tables")) };
            let mut table = toml_edit::Table::new();
            for (key, value) in step[2].as_object().into_iter().flatten() {
                table.insert(key, toml_edit::value(edit_value(value)?));
            }
            array.push(table);
            Ok(json!(array.len()))
        }
        "aot_remove" => {
            let index = step[2].as_u64().unwrap_or_default() as usize;
            let Some(item) = item_at(document, path) else { return Ok(json!("missing")) };
            let Some(array) = item.as_array_of_tables_mut() else { return Ok(json!("not an array of tables")) };
            if index >= array.len() {
                return Ok(json!("out of bounds"));
            }
            array.remove(index);
            Ok(json!(array.len()))
        }
        "make_value" => {
            let Some(item) = item_at(document, path) else { return Ok(json!("missing")) };
            item.make_value();
            Ok(json!(item.type_name()))
        }
        "into_table" => {
            let Some(item) = item_at(document, path) else { return Ok(json!("missing")) };
            let taken = std::mem::take(item);
            *item = match taken.into_table() {
                Ok(table) => Item::Table(table),
                Err(other) => other,
            };
            Ok(json!(item.type_name()))
        }
        "into_array_of_tables" => {
            let Some(item) = item_at(document, path) else { return Ok(json!("missing")) };
            let taken = std::mem::take(item);
            *item = match taken.into_array_of_tables() {
                Ok(array) => Item::ArrayOfTables(array),
                Err(other) => other,
            };
            Ok(json!(item.type_name()))
        }
        "set_trailing" => {
            document.set_trailing(step[2].as_str().unwrap_or_default());
            Ok(json!("done"))
        }
        "display" => {
            let Some(item) = item_at(document, path) else { return Ok(json!("missing")) };
            Ok(json!(item.to_string()))
        }
        _ => Err("unknown step".to_owned()),
    }
}

// The save path of mj-core/src/config/document.rs, without the settings that
// are particular to Mjolnir's own schema: what remains is the general
// algorithm that brings a file from one serialization to another while
// leaving what did not change as the user wrote it.

fn edit_in_place(existing: &str, loaded: &str, updated: &str) -> Option<String> {
    let mut document = existing.parse::<Document>().ok()?;
    let raw = existing.parse::<toml::Table>().ok()?;
    let loaded = loaded.parse::<toml::Table>().ok()?;
    let updated_table = updated.parse::<toml::Table>().ok()?;
    let source = updated.parse::<Document>().ok()?;
    let mut next_position = usize::MAX / 2;
    apply(
        document.as_table_mut(),
        Tables {
            raw: Some(&raw),
            before: Some(&loaded),
            after: &updated_table,
        },
        source.as_table(),
        true,
        &mut next_position,
    );
    if let Some(version) = updated_table.get("version")
        && document.get("version").and_then(Item::as_integer) != version.as_integer()
        && let Some(item) = document.get_mut("version")
        && let Some(value) = item.as_value_mut()
    {
        let decor = value.decor().clone();
        *value = toml_edit::Value::from(version.as_integer().unwrap_or_default());
        *value.decor_mut() = decor;
    }
    Some(document.to_string())
}

#[derive(Clone, Copy)]
struct Tables<'a> {
    raw: Option<&'a toml::Table>,
    before: Option<&'a toml::Table>,
    after: &'a toml::Table,
}

fn apply(
    target: &mut dyn TableLike,
    tables: Tables<'_>,
    source: &dyn TableLike,
    top_level: bool,
    next_position: &mut usize,
) {
    let Tables { raw, before, after } = tables;
    let stale = target
        .iter()
        .map(|(key, _)| key.to_owned())
        .filter(|key| {
            !after.contains_key(key) && (top_level || before.is_some_and(|before| before.contains_key(key)))
        })
        .collect::<Vec<_>>();
    for key in stale {
        target.remove(&key);
    }
    for (key, value) in after {
        let previous = before.and_then(|before| before.get(key));
        let spelled = raw.and_then(|raw| raw.get(key));
        if previous == Some(value) && spelled.is_none_or(|spelled| spelled == value) {
            continue;
        }
        let Some(replacement) = source.get(key) else {
            continue;
        };
        match target.get_mut(key) {
            Some(existing) => {
                if let toml::Value::Table(value) = value
                    && let Some(table) = existing.as_table_like_mut()
                    && let Some(source) = replacement.as_table_like()
                {
                    let tables = Tables {
                        raw: spelled.and_then(toml::Value::as_table),
                        before: previous.and_then(toml::Value::as_table),
                        after: value,
                    };
                    apply(table, tables, source, false, next_position);
                } else if let Some(existing) = existing.as_value_mut()
                    && let Ok(mut value) = replacement.clone().into_value()
                {
                    *value.decor_mut() = existing.decor().clone();
                    *existing = value;
                } else {
                    *existing = placed_last(replacement.clone(), next_position);
                }
            }
            None => {
                target.insert(key, placed_last(replacement.clone(), next_position));
            }
        }
    }
}

fn placed_last(mut item: Item, next_position: &mut usize) -> Item {
    place(&mut item, next_position);
    item
}

fn place(item: &mut Item, next_position: &mut usize) {
    match item {
        Item::Table(table) => {
            table.set_position(*next_position);
            *next_position += 1;
            for (_, child) in table.iter_mut() {
                place(child, next_position);
            }
        }
        Item::ArrayOfTables(array) => {
            for table in array.iter_mut() {
                table.set_position(*next_position);
                *next_position += 1;
                for (_, child) in table.iter_mut() {
                    place(child, next_position);
                }
            }
        }
        _ => {}
    }
}

// Types defined the same way in go/crates/toml/oracle_derive_test.go, in the
// shapes Mjolnir's configuration uses: options with defaults and
// skip_serializing_if, renamed fields and aliases, deny_unknown_fields,
// enums in each representation, flatten inside an internally tagged
// variant, nested tables, arrays of tables, datetimes and toml values.
mod derived {
    use std::collections::BTreeMap;
    use std::fmt::Debug;

    use serde::de::DeserializeOwned;
    use serde::{Deserialize, Serialize};
    use serde_json::{Value, json};

    pub fn run(type_name: &str, text: &str) -> Option<Value> {
        Some(match type_name {
            "Config" => round_trip::<Config>(text),
            "Profile" => round_trip::<Profile>(text),
            "Target" => round_trip::<Target>(text),
            "Theme" => round_trip::<Theme>(text),
            "Daemon" => round_trip::<Daemon>(text),
            "Adjacent" => round_trip::<Adjacent>(text),
            "Holder" => round_trip::<Holder>(text),
            _ => return None,
        })
    }

    fn round_trip<T: Serialize + DeserializeOwned + Debug>(text: &str) -> Value {
        match toml::from_str::<T>(text) {
            Ok(value) => json!({"ok": {
                "debug": format!("{value:?}"),
                "to_string": super::ser_result(toml::to_string(&value)),
                "to_string_pretty": super::ser_result(toml::to_string_pretty(&value)),
                "value": match toml::Value::try_from(&value) {
                    Ok(value) => json!({"ok": super::to_canonical(&value)}),
                    Err(error) => json!({"err": error.to_string()}),
                },
            }}),
            Err(error) => super::de_error(&error),
        }
    }

    fn default_true() -> bool {
        true
    }

    fn is_true(value: &bool) -> bool {
        *value
    }

    fn default_port() -> u16 {
        22
    }

    #[derive(Debug, Serialize, Deserialize)]
    pub struct Config {
        pub version: u32,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        pub name: Option<String>,
        #[serde(default = "default_port")]
        pub port: u16,
        #[serde(default, skip_serializing_if = "Vec::is_empty")]
        pub tags: Vec<String>,
        #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
        pub profiles: BTreeMap<String, Profile>,
        #[serde(default, skip_serializing_if = "Vec::is_empty")]
        pub targets: Vec<Target>,
        #[serde(default)]
        pub daemon: Daemon,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        pub theme: Option<Theme>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        pub ratio: Option<f64>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        pub when: Option<toml::value::Datetime>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        pub binding: Option<Binding>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        pub adjacent: Option<Adjacent>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        pub extra: Option<toml::Table>,
    }

    #[derive(Debug, Serialize, Deserialize)]
    #[serde(deny_unknown_fields)]
    pub struct Profile {
        pub harness: Harness,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        pub model: Option<String>,
        #[serde(default = "default_true", skip_serializing_if = "is_true")]
        pub enabled: bool,
        #[serde(default, rename = "max-turns", alias = "max_turns", skip_serializing_if = "Option::is_none")]
        pub max_turns: Option<i64>,
    }

    #[derive(Debug, Serialize, Deserialize)]
    #[serde(rename_all = "kebab-case")]
    pub enum Harness {
        ClaudeCode,
        Codex,
        OpenCode,
    }

    #[derive(Debug, Serialize, Deserialize)]
    pub struct SshConnection {
        pub host: String,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        pub user: Option<String>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        pub port: Option<u16>,
    }

    #[derive(Debug, Serialize, Deserialize)]
    #[serde(tag = "kind", rename_all = "snake_case")]
    pub enum Target {
        Local {
            name: String,
        },
        Ssh {
            name: String,
            #[serde(flatten)]
            ssh: SshConnection,
        },
        Container {
            name: String,
            image: String,
            #[serde(default)]
            args: Vec<String>,
        },
    }

    #[derive(Debug, Default, Serialize, Deserialize)]
    #[serde(default)]
    pub struct Daemon {
        pub remote_command: Option<String>,
        pub listen: Option<Listen>,
        pub verbose: bool,
    }

    #[derive(Debug, Serialize, Deserialize)]
    #[serde(untagged)]
    pub enum Listen {
        Port(u16),
        Address(String),
        Detailed { host: String, port: u16 },
    }

    #[derive(Debug, Serialize, Deserialize)]
    #[serde(untagged)]
    pub enum Binding {
        One(String),
        Many(Vec<String>),
    }

    #[derive(Debug, Serialize, Deserialize)]
    pub enum Theme {
        Dark,
        Light,
        Custom(String),
        Rgb(u8, u8, u8),
        Named {
            name: String,
            #[serde(default)]
            bold: bool,
        },
    }

    #[derive(Debug, Serialize, Deserialize)]
    #[serde(tag = "t", content = "c")]
    pub enum Adjacent {
        A,
        B(i64),
        C { x: i64 },
    }

    /// Each enum as the value of a key, where toml reads it through its
    /// value deserializer rather than the document's.
    #[derive(Debug, Serialize, Deserialize)]
    pub struct Holder {
        #[serde(default, skip_serializing_if = "Option::is_none")]
        pub theme: Option<Theme>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        pub target: Option<Target>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        pub adjacent: Option<Adjacent>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        pub harness: Option<Harness>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        pub listen: Option<Listen>,
        #[serde(default, skip_serializing_if = "Vec::is_empty")]
        pub themes: Vec<Theme>,
    }
}
