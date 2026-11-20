#![allow(dead_code)]
//! Oracle module `serde_derive`: serde 1.0.229's derive behaviour, its data
//! model and serde_ignored 0.1.14, as go/crates/serde reproduces them.
//!
//! Requests are objects with an `"op"` member. The operations whose name
//! starts with `misc_` are served by `serde_derive_misc.rs`; the Go tests that
//! send each operation document it (go/crates/serde/oracle_test.go).
//!
//! `derive` deserializes the input as one of the types below and, when that
//! succeeds, serializes the value again:
//!
//! ```text
//! {"op": "derive", "type": "Plain", "json": "..", "mode": "str"}
//! {"op": "derive", "type": "Plain", "bytes": [..], "mode": "slice"}
//! -> {"ok": {"compact": "..", "pretty": "..", "value": ".."}}
//! -> {"err": "..", "class": "Data", "line": 1, "column": 5}
//! ```
//!
//! `mode` is `str` (from_str), `slice` (from_slice), `reader` (from_reader)
//! or `value` (the input parsed as a Value, then from_value). Each type has
//! an identical Go declaration in go/crates/serde/oracle_types_test.go.

use serde::{Deserialize, Deserializer, Serialize, Serializer};
use serde_json::{Map, Value, json};
use std::collections::{BTreeMap, BTreeSet, HashMap};
use std::path::PathBuf;
use std::time::{Duration, SystemTime};

#[path = "serde_derive_misc.rs"]
mod misc;

pub fn handle(request: &Value) -> Value {
    let op = crate::text(request, "op");
    if op.starts_with("misc_") {
        return misc::handle(request);
    }
    match op {
        "derive" => derive(request),
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

/// Deserializes the input as T in the request's mode, and serializes what
/// came out.
pub fn run<T: Serialize + for<'de> Deserialize<'de>>(request: &Value) -> Value {
    let input = input(request);
    let result: Result<T, serde_json::Error> = match crate::text(request, "mode") {
        "slice" => serde_json::from_slice(&input),
        "reader" => serde_json::from_reader(&input[..]),
        "value" => match serde_json::from_slice::<Value>(&input) {
            Ok(value) => serde_json::from_value(value),
            Err(error) => return json!({"value_err": error.to_string()}),
        },
        _ => match std::str::from_utf8(&input) {
            Ok(text) => serde_json::from_str(text),
            Err(_) => return json!({"oracle_error": "str input is not UTF-8"}),
        },
    };
    match result {
        Ok(value) => {
            let compact = match serde_json::to_string(&value) {
                Ok(text) => json!(text),
                Err(error) => json!({"ser_err": error.to_string()}),
            };
            let pretty = match serde_json::to_string_pretty(&value) {
                Ok(text) => json!(text),
                Err(error) => json!({"ser_err": error.to_string()}),
            };
            let as_value = match serde_json::to_value(&value) {
                Ok(value) => json!(value.to_string()),
                Err(error) => json!({"ser_err": error.to_string()}),
            };
            json!({"ok": {"compact": compact, "pretty": pretty, "value": as_value}})
        }
        Err(error) => error_reply(&error),
    }
}

macro_rules! types {
    ($request:expr, $name:expr; $($ty:ident),* $(,)?) => {
        match $name {
            $(stringify!($ty) => run::<$ty>($request),)*
            _ => crate::unknown($request),
        }
    };
}

fn derive(request: &Value) -> Value {
    types!(request, crate::text(request, "type");
        Plain, Numbers, Paths, Collections, Nested, Renamed, RenamedCamel,
        RenamedPascal, RenamedScreaming, RenamedKebab, RenamedScreamingKebab,
        RenamedLower, RenamedUpper, Strict, Defaults, ContainerDefault,
        StrictDefaults, Skips, SkipIfCustom, Aliases, Flattened, FlattenMap,
        FlattenStrict, FlattenOption, Transparent, Newtype, TupleStruct, UnitStruct,
        External, ExternalRenamed, ExternalOther, Internal, InternalStrict,
        Adjacent, AdjacentStrict, Untagged, Mixed, CEnum, CEnumRenamed, FromInto,
        TryFromType, WithFns, Tree, Times, Values, RawHolder, IntKeys, Bytes,
        TaggedStruct, Optional, VariantFlatten, InternalNewtype, Wrapper,
        UsizeMax, U8Seq, Tuples, F32s,
    )
}

#[derive(Debug, Serialize, Deserialize)]
pub struct Plain {
    pub flag: bool,
    pub count: u32,
    pub name: String,
    pub maybe: Option<String>,
    pub list: Vec<i64>,
    pub ratio: f64,
}

#[derive(Debug, Serialize, Deserialize)]
pub struct Numbers {
    pub a: i8,
    pub b: i16,
    pub c: i32,
    pub d: i64,
    pub e: u8,
    pub f: u16,
    pub g: u32,
    pub h: u64,
    pub i: usize,
    pub j: f32,
    pub k: f64,
    pub l: isize,
    pub m: char,
}

#[derive(Debug, Serialize, Deserialize)]
pub struct Paths {
    pub path: PathBuf,
    pub paths: Vec<PathBuf>,
    pub maybe: Option<PathBuf>,
    pub by_name: BTreeMap<String, PathBuf>,
}

#[derive(Debug, Serialize, Deserialize)]
pub struct Collections {
    pub tree: BTreeMap<String, u32>,
    pub set: BTreeSet<String>,
    pub hash: HashMap<String, bool>,
    pub nested: Vec<Vec<u8>>,
    pub pairs: Vec<(String, i32)>,
}

#[derive(Debug, Serialize, Deserialize)]
pub struct Nested {
    pub plain: Plain,
    pub items: Vec<Numbers>,
    pub boxed: Box<Plain>,
}

#[derive(Debug, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub struct Renamed {
    pub first_field: u8,
    #[serde(rename = "Explicit")]
    pub second_field: u8,
    pub r#type: u8,
}

#[derive(Debug, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct RenamedCamel {
    pub first_field: u8,
    pub a_b_c: u8,
    pub x: u8,
}

#[derive(Debug, Serialize, Deserialize)]
#[serde(rename_all = "PascalCase")]
pub struct RenamedPascal {
    pub first_field: u8,
    pub x: u8,
}

#[derive(Debug, Serialize, Deserialize)]
#[serde(rename_all = "SCREAMING_SNAKE_CASE")]
pub struct RenamedScreaming {
    pub first_field: u8,
}

#[derive(Debug, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub struct RenamedKebab {
    pub first_field: u8,
    pub second: u8,
}

#[derive(Debug, Serialize, Deserialize)]
#[serde(rename_all = "SCREAMING-KEBAB-CASE")]
pub struct RenamedScreamingKebab {
    pub first_field: u8,
}

#[derive(Debug, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub struct RenamedLower {
    pub first_field: u8,
}

#[derive(Debug, Serialize, Deserialize)]
#[serde(rename_all = "UPPERCASE")]
pub struct RenamedUpper {
    pub first_field: u8,
}

#[derive(Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Strict {
    pub a: u8,
    pub b: Option<u8>,
}

fn default_seven() -> u32 {
    7
}

fn default_name() -> String {
    "unnamed".to_owned()
}

#[derive(Debug, Serialize, Deserialize)]
pub struct Defaults {
    #[serde(default)]
    pub count: u32,
    #[serde(default = "default_seven")]
    pub seven: u32,
    #[serde(default = "default_name")]
    pub name: String,
    #[serde(default)]
    pub list: Vec<String>,
    #[serde(default)]
    pub inner: Plain2,
}

#[derive(Debug, Default, Serialize, Deserialize)]
pub struct Plain2 {
    pub x: u8,
    pub y: String,
}

#[derive(Debug, Serialize, Deserialize)]
#[serde(default)]
pub struct ContainerDefault {
    pub a: u32,
    pub b: String,
    #[serde(skip)]
    pub skipped: u32,
}

impl Default for ContainerDefault {
    fn default() -> Self {
        ContainerDefault {
            a: 11,
            b: "dflt".to_owned(),
            skipped: 13,
        }
    }
}

#[derive(Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields, default)]
pub struct StrictDefaults {
    pub a: u32,
    pub b: Vec<u8>,
}

impl Default for StrictDefaults {
    fn default() -> Self {
        StrictDefaults { a: 1, b: vec![2] }
    }
}

#[derive(Debug, Serialize, Deserialize)]
pub struct Skips {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub maybe: Option<u8>,
    #[serde(skip_serializing_if = "Vec::is_empty", default)]
    pub list: Vec<u8>,
    #[serde(skip_serializing_if = "String::is_empty", default)]
    pub text: String,
    #[serde(skip)]
    pub hidden: u8,
    #[serde(skip_serializing)]
    pub write_only: u8,
    #[serde(skip_deserializing)]
    pub read_only: u8,
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub flag: bool,
    #[serde(skip_serializing_if = "BTreeMap::is_empty", default)]
    pub map: BTreeMap<String, u8>,
}

fn is_zero(value: &u32) -> bool {
    *value == 0
}

#[derive(Debug, Serialize, Deserialize)]
pub struct SkipIfCustom {
    #[serde(default, skip_serializing_if = "is_zero")]
    pub count: u32,
    pub name: String,
}

#[derive(Debug, Serialize, Deserialize)]
pub struct Aliases {
    #[serde(alias = "old_name", alias = "older")]
    pub name: String,
    #[serde(rename = "ren", alias = "zz")]
    pub renamed: u8,
}

#[derive(Debug, Serialize, Deserialize)]
pub struct Flattened {
    pub id: u32,
    #[serde(flatten)]
    pub inner: Plain2,
    #[serde(flatten)]
    pub rest: BTreeMap<String, Value>,
}

#[derive(Debug, Serialize, Deserialize)]
pub struct FlattenMap {
    pub kind: String,
    #[serde(flatten)]
    pub extra: Map<String, Value>,
}

#[derive(Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct FlattenStrict {
    pub id: u32,
    #[serde(flatten)]
    pub inner: Plain2,
}

#[derive(Debug, Serialize, Deserialize)]
pub struct FlattenOption {
    pub id: u32,
    #[serde(flatten)]
    pub inner: Option<Plain2>,
}

#[derive(Debug, Serialize, Deserialize)]
#[serde(transparent)]
pub struct Transparent {
    pub inner: Vec<u32>,
}

#[derive(Debug, Serialize, Deserialize)]
pub struct Newtype(pub String);

#[derive(Debug, Serialize, Deserialize)]
pub struct TupleStruct(pub u8, pub String, pub Option<bool>);

#[derive(Debug, Serialize, Deserialize)]
pub struct UnitStruct;

#[derive(Debug, Serialize, Deserialize)]
pub enum External {
    Unit,
    Newtype(u32),
    Tuple(u8, String),
    Struct { a: u8, b: Option<String> },
    #[serde(skip)]
    Skipped,
    Empty {},
    EmptyTuple(),
}

#[derive(Debug, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ExternalRenamed {
    FirstVariant,
    #[serde(rename = "SECOND", alias = "second_alias")]
    SecondVariant(u8),
    #[serde(rename_all = "camelCase")]
    ThirdVariant {
        some_field: u8,
    },
    #[serde(skip_serializing)]
    WriteOnly,
    #[serde(skip_deserializing)]
    ReadOnly,
}

#[derive(Debug, Serialize, Deserialize)]
pub enum ExternalOther {
    Known,
    #[serde(other)]
    Unknown,
}

#[derive(Debug, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum Internal {
    Unit,
    Struct { a: u8, b: Option<String> },
    Newtype(Plain2),
    NewtypeMap(BTreeMap<String, u8>),
    #[serde(rename = "other_name")]
    Renamed { x: bool },
}

#[derive(Debug, Serialize, Deserialize)]
#[serde(tag = "kind", deny_unknown_fields)]
pub enum InternalStrict {
    A { x: u8 },
    B,
}

#[derive(Debug, Serialize, Deserialize)]
#[serde(tag = "t", content = "c")]
pub enum Adjacent {
    Unit,
    Newtype(u32),
    Tuple(u8, u8),
    Struct { a: u8 },
    Optional(Option<u8>),
}

#[derive(Debug, Serialize, Deserialize)]
#[serde(tag = "t", content = "c", deny_unknown_fields)]
pub enum AdjacentStrict {
    Unit,
    Newtype(u32),
}

#[derive(Debug, Serialize, Deserialize)]
#[serde(untagged)]
pub enum Untagged {
    Int(u32),
    Text(String),
    Pair(u8, u8),
    Fields { a: u8, b: u8 },
    Plain(Plain2),
    Nothing,
}

#[derive(Debug, Serialize, Deserialize)]
pub enum Mixed {
    Tagged(u8),
    Other { x: u8 },
    #[serde(untagged)]
    Loose(String),
    #[serde(untagged)]
    LooseNumber(f64),
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize)]
pub enum CEnum {
    Alpha,
    Beta,
    Gamma,
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum CEnumRenamed {
    FirstOne,
    SecondOne,
    #[serde(alias = "third")]
    ThirdOne,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(from = "FromIntoRepr", into = "FromIntoRepr")]
pub struct FromInto {
    pub value: u32,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct FromIntoRepr {
    pub raw: String,
}

impl From<FromIntoRepr> for FromInto {
    fn from(repr: FromIntoRepr) -> Self {
        FromInto {
            value: repr.raw.len() as u32,
        }
    }
}

impl From<FromInto> for FromIntoRepr {
    fn from(value: FromInto) -> Self {
        FromIntoRepr {
            raw: "x".repeat(value.value as usize),
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(try_from = "String")]
pub struct TryFromType {
    pub text: String,
}

impl TryFrom<String> for TryFromType {
    type Error = String;

    fn try_from(text: String) -> Result<Self, String> {
        if text.is_empty() {
            Err("text must not be empty".to_owned())
        } else {
            Ok(TryFromType { text })
        }
    }
}

mod doubled {
    use serde::{Deserialize, Deserializer, Serializer};

    pub fn serialize<S: Serializer>(value: &u32, serializer: S) -> Result<S::Ok, S::Error> {
        serializer.serialize_u64(u64::from(*value) * 2)
    }

    pub fn deserialize<'de, D: Deserializer<'de>>(deserializer: D) -> Result<u32, D::Error> {
        let doubled = u64::deserialize(deserializer)?;
        if doubled % 2 != 0 {
            return Err(serde::de::Error::custom(format!("{doubled} is odd")));
        }
        Ok((doubled / 2) as u32)
    }
}

fn upper<S: Serializer>(value: &str, serializer: S) -> Result<S::Ok, S::Error> {
    serializer.serialize_str(&value.to_uppercase())
}

fn lower<'de, D: Deserializer<'de>>(deserializer: D) -> Result<String, D::Error> {
    let text = String::deserialize(deserializer)?;
    Ok(text.to_lowercase())
}

#[derive(Debug, Serialize, Deserialize)]
pub struct WithFns {
    #[serde(with = "doubled")]
    pub doubled: u32,
    #[serde(serialize_with = "upper", deserialize_with = "lower")]
    pub text: String,
    #[serde(default, deserialize_with = "lower")]
    pub optional_text: String,
}

#[derive(Debug, Serialize, Deserialize)]
pub struct Tree {
    pub label: String,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub children: Vec<Tree>,
}

#[derive(Debug, Serialize, Deserialize)]
pub struct Times {
    pub duration: Duration,
    pub time: SystemTime,
}

#[derive(Debug, Serialize, Deserialize)]
pub struct Values {
    pub value: Value,
    pub map: Map<String, Value>,
    pub list: Vec<Value>,
    pub maybe: Option<Value>,
}

#[derive(Debug, Serialize, Deserialize)]
pub struct RawHolder {
    pub id: u8,
    pub raw: Box<serde_json::value::RawValue>,
}

#[derive(Debug, Serialize, Deserialize)]
pub struct IntKeys {
    pub by_number: BTreeMap<u32, String>,
    pub by_signed: BTreeMap<i64, bool>,
    pub by_bool: BTreeMap<bool, u8>,
}

#[derive(Debug, Serialize, Deserialize)]
pub struct Bytes {
    pub data: Vec<u8>,
}

#[derive(Debug, Serialize, Deserialize)]
#[serde(tag = "type")]
pub struct TaggedStruct {
    pub a: u8,
}

#[derive(Debug, Serialize, Deserialize)]
pub struct Optional {
    pub one: Option<u8>,
    pub two: Option<Option<u8>>,
    #[serde(default)]
    pub three: Option<Vec<u8>>,
}

#[derive(Debug, Serialize, Deserialize)]
pub enum VariantFlatten {
    Holder {
        id: u8,
        #[serde(flatten)]
        rest: BTreeMap<String, u8>,
    },
}

#[derive(Debug, Serialize, Deserialize)]
#[serde(tag = "op")]
pub enum InternalNewtype {
    Nested(Internal),
    Unitish(UnitStruct),
    Maybe(Option<u8>),
}

#[derive(Debug, Serialize, Deserialize)]
pub struct Wrapper {
    pub external: Vec<External>,
    pub internal: Vec<Internal>,
    pub adjacent: Vec<Adjacent>,
    pub untagged: Vec<Untagged>,
    pub cenum: Vec<CEnum>,
}

#[derive(Debug, Serialize, Deserialize)]
pub struct UsizeMax {
    pub size: usize,
}

#[derive(Debug, Serialize, Deserialize)]
pub struct U8Seq {
    pub array: [u8; 3],
    pub empty: [u8; 0],
}

#[derive(Debug, Serialize, Deserialize)]
pub struct Tuples {
    pub pair: (u8, String),
    pub triple: (bool, i32, Option<u8>),
    pub unit: (),
}

#[derive(Debug, Serialize, Deserialize)]
pub struct F32s {
    pub single: f32,
    pub list: Vec<f32>,
}
