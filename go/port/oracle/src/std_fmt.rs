//! Oracle module `std_fmt`: what `core::fmt` and `alloc::fmt` print.
//!
//! The Go formatter (`go/rs/fmt_*.go`) reads Rust format strings at run time;
//! `format!` wants its format string at compile time. So this module compiles
//! one `format!` for every combination of fill, alignment, sign, `#`, `0` and
//! type, with the width and the precision passed as arguments, and a request
//! picks the combination by its text.
//!
//! Requests (see `go/rs/fmt_oracle_test.go`):
//!
//! - `{"op":"fmt","v":VALUE,"specs":[[FLAGS,TYPE,WIDTH,PRECISION],..]}` formats
//!   one value with each spec and replies `{"out":[..]}`. FLAGS is
//!   `[[fill]align][sign]['#']['0']`, TYPE is `` `?` `x?` `X?` `x` `X` `o` `b`
//!   `e` `E`, WIDTH and PRECISION are numbers or null. Members of a spec after
//!   the fourth are for the Go side only.
//! - `{"op":"tmpl","f":FORMAT,"args":ARGS}` formats one of the fixed format
//!   strings in `template` and replies `{"out":".."}`. ARGS is the argument
//!   list as it is written there (`a, n = b`), which tells apart two uses of
//!   one format string; other members are for the Go side only.
//! - `{"op":"escaped"}` replies with the ranges of chars that `{:?}` writes as
//!   `\u{..}`, and `{"op":"whitespace"}` with those `char::is_whitespace`
//!   holds for.
//!
//! VALUE is `[TAG, ..]`; `value` below lists the tags.

use std::collections::{BTreeMap, BTreeSet, HashMap, VecDeque};
use std::fmt::{self, Binary, Debug, Display, LowerExp, LowerHex, Octal, UpperExp, UpperHex};
use std::sync::OnceLock;
use std::time::Duration;

use serde_json::{Value, json};

pub fn handle(request: &Value) -> Value {
    match crate::text(request, "op") {
        "fmt" => {
            let value = value(&request["v"]);
            let specs = request["specs"].as_array().expect("specs");
            let out: Vec<String> = specs.iter().map(|spec| format_spec(&value, spec)).collect();
            json!({ "out": out })
        }
        "tmpl" => match template(crate::text(request, "f"), crate::text(request, "args")) {
            Some(out) => json!({ "out": out }),
            None => crate::unknown(request),
        },
        "escaped" => json!({ "ranges": char_ranges(|c| format!("{c:?}").starts_with("'\\u{")) }),
        "whitespace" => json!({ "ranges": char_ranges(char::is_whitespace) }),
        _ => crate::unknown(request),
    }
}

/// The inclusive ranges of chars a predicate holds for.
fn char_ranges(holds: impl Fn(char) -> bool) -> Vec<(u32, u32)> {
    let mut ranges: Vec<(u32, u32)> = Vec::new();
    for c in '\0'..=char::MAX {
        if !holds(c) {
            continue;
        }
        match ranges.last_mut() {
            Some(last) if last.1 + 1 == c as u32 => last.1 = c as u32,
            _ => ranges.push((c as u32, c as u32)),
        }
    }
    ranges
}

// The compiled format strings.

/// The compiled `format!` calls of one trait, by spec.
macro_rules! Table {
    ($tr:path) => {
        HashMap<&'static str, for<'a> fn(&'a (dyn $tr + 'a), usize, usize) -> String>
    };
}

/// One flag combination: the four forms with and without a width and a
/// precision, under the key `FLAGS|TYPE|` followed by `w`, `p`, both or
/// neither.
macro_rules! leaf {
    ($map:ident, $tr:path, $ty:literal, $fa:literal, $sign:literal, $alt:literal, $zero:literal) => {
        $map.insert(concat!($fa, $sign, $alt, $zero, "|", $ty, "|"), |v: &dyn $tr, _w: usize, _p: usize| {
            format!(concat!("{0:", $fa, $sign, $alt, $zero, $ty, "}"), v)
        });
        $map.insert(concat!($fa, $sign, $alt, $zero, "|", $ty, "|w"), |v: &dyn $tr, w: usize, _p: usize| {
            format!(concat!("{0:", $fa, $sign, $alt, $zero, "1$", $ty, "}"), v, w)
        });
        $map.insert(concat!($fa, $sign, $alt, $zero, "|", $ty, "|p"), |v: &dyn $tr, _w: usize, p: usize| {
            format!(concat!("{0:", $fa, $sign, $alt, $zero, ".1$", $ty, "}"), v, p)
        });
        $map.insert(concat!($fa, $sign, $alt, $zero, "|", $ty, "|wp"), |v: &dyn $tr, w: usize, p: usize| {
            format!(concat!("{0:", $fa, $sign, $alt, $zero, "1$.2$", $ty, "}"), v, w, p)
        });
    };
}

macro_rules! zeros {
    ($map:ident, $tr:path, $ty:literal, $fa:literal, $sign:literal, $alt:literal) => {
        leaf!($map, $tr, $ty, $fa, $sign, $alt, "");
        leaf!($map, $tr, $ty, $fa, $sign, $alt, "0");
    };
}

macro_rules! alts {
    ($map:ident, $tr:path, $ty:literal, $fa:literal, $sign:literal) => {
        zeros!($map, $tr, $ty, $fa, $sign, "");
        zeros!($map, $tr, $ty, $fa, $sign, "#");
    };
}

macro_rules! signs {
    ($map:ident, $tr:path, $ty:literal, $fa:literal) => {
        alts!($map, $tr, $ty, $fa, "");
        alts!($map, $tr, $ty, $fa, "+");
        alts!($map, $tr, $ty, $fa, "-");
    };
}

/// Every flag combination for one trait and type.
macro_rules! table {
    ($tr:path, $ty:literal) => {{
        let mut map: Table!($tr) = HashMap::new();
        signs!(map, $tr, $ty, "");
        signs!(map, $tr, $ty, "<");
        signs!(map, $tr, $ty, "^");
        signs!(map, $tr, $ty, ">");
        signs!(map, $tr, $ty, "*<");
        signs!(map, $tr, $ty, "*^");
        signs!(map, $tr, $ty, "*>");
        signs!(map, $tr, $ty, "0<");
        signs!(map, $tr, $ty, "0^");
        signs!(map, $tr, $ty, "0>");
        signs!(map, $tr, $ty, "é<");
        signs!(map, $tr, $ty, "é^");
        signs!(map, $tr, $ty, "é>");
        map
    }};
}

struct Tables {
    display: Table!(Display),
    debug: Table!(Debug),
    debug_lower_hex: Table!(Debug),
    debug_upper_hex: Table!(Debug),
    lower_hex: Table!(LowerHex),
    upper_hex: Table!(UpperHex),
    octal: Table!(Octal),
    binary: Table!(Binary),
    lower_exp: Table!(LowerExp),
    upper_exp: Table!(UpperExp),
}

fn tables() -> &'static Tables {
    static TABLES: OnceLock<Tables> = OnceLock::new();
    TABLES.get_or_init(|| Tables {
        display: table!(Display, ""),
        debug: table!(Debug, "?"),
        debug_lower_hex: table!(Debug, "x?"),
        debug_upper_hex: table!(Debug, "X?"),
        lower_hex: table!(LowerHex, "x"),
        upper_hex: table!(UpperHex, "X"),
        octal: table!(Octal, "o"),
        binary: table!(Binary, "b"),
        lower_exp: table!(LowerExp, "e"),
        upper_exp: table!(UpperExp, "E"),
    })
}

fn format_spec(value: &Val, spec: &Value) -> String {
    let flags = spec[0].as_str().expect("flags");
    let ty = spec[1].as_str().expect("type");
    let width = spec[2].as_u64().map(|w| w as usize);
    let precision = spec[3].as_u64().map(|p| p as usize);
    let key = format!(
        "{flags}|{ty}|{}{}",
        if width.is_some() { "w" } else { "" },
        if precision.is_some() { "p" } else { "" }
    );
    let (w, p) = (width.unwrap_or(0), precision.unwrap_or(0));
    let tables = tables();
    macro_rules! run {
        ($table:ident, $object:expr) => {{
            let format = tables.$table.get(key.as_str()).unwrap_or_else(|| panic!("oracle: no spec `{key}`"));
            let object = $object.unwrap_or_else(|| panic!("oracle: the value has no `{ty}` form"));
            format(object, w, p)
        }};
    }
    match ty {
        "" => run!(display, value.display()),
        "?" => run!(debug, Some(value as &dyn Debug)),
        "x?" => run!(debug_lower_hex, Some(value as &dyn Debug)),
        "X?" => run!(debug_upper_hex, Some(value as &dyn Debug)),
        "x" => run!(lower_hex, value.lower_hex()),
        "X" => run!(upper_hex, value.upper_hex()),
        "o" => run!(octal, value.octal()),
        "b" => run!(binary, value.binary()),
        "e" => run!(lower_exp, value.lower_exp()),
        "E" => run!(upper_exp, value.upper_exp()),
        _ => panic!("oracle: no type `{ty}`"),
    }
}

// The values.

#[derive(Debug, Clone)]
struct UnitStruct;

#[derive(Debug, Clone)]
struct Empty {}

#[derive(Debug, Clone)]
struct Point {
    x: i32,
    y: i32,
}

#[derive(Debug, Clone)]
struct Pair(Val, Val);

#[derive(Debug, Clone)]
struct Wrap(Val);

#[derive(Debug, Clone)]
struct SessionId(String);

#[derive(Debug, Clone)]
struct Meters(f64);

#[derive(Debug, Clone)]
struct Count(u32);

#[derive(Debug, Clone)]
struct Tags(Vec<String>);

#[derive(Debug, Clone)]
enum Kind {
    Alpha,
    Beta,
    GammaDelta,
}

#[derive(Debug, Clone)]
enum State {
    Idle,
    Running { pid: u32, since: Duration },
    Failed(String, i32),
    Wrapped(Box<State>),
    Maybe(Option<u8>),
    Payload(Val),
}

#[derive(Debug, Clone)]
struct Record {
    id: u64,
    name: String,
    tags: Vec<String>,
    parent: Option<Box<Record>>,
    scores: BTreeMap<String, f64>,
    kind: Kind,
    state: State,
    raw: Vec<u8>,
    elapsed: Duration,
    pair: (i32, String),
    ch: char,
    unit: (),
    flag: bool,
    ratio: f32,
    session: SessionId,
    private_note: Option<String>,
    any: Val,
}

/// An error with a derived Debug, for `Err(..)`.
#[derive(Debug, Clone)]
struct ParseFailure {
    line: u32,
    reason: String,
}

/// A value described by a request. Its Debug is the Debug of what it holds.
#[derive(Clone)]
enum Val {
    I8(i8),
    I16(i16),
    I32(i32),
    I64(i64),
    I128(i128),
    Isize(isize),
    U8(u8),
    U16(u16),
    U32(u32),
    U64(u64),
    U128(u128),
    Usize(usize),
    F32(f32),
    F64(f64),
    Bool(bool),
    Char(char),
    Str(String),
    Unit,
    Dur(Duration),
    Opt(Option<Box<Val>>),
    Res(Result<Box<Val>, ParseFailure>),
    Vec(Vec<Val>),
    Deque(VecDeque<Val>),
    Bytes(Vec<u8>),
    VecI32(Vec<i32>),
    VecStr(Vec<String>),
    Arr3([i32; 3]),
    T2(Box<(Val, Val)>),
    T3(Box<(Val, Val, Val)>),
    T4(Box<(Val, Val, Val, Val)>),
    T5(Box<(Val, Val, Val, Val, Val)>),
    MapStr(BTreeMap<String, Val>),
    MapI64(BTreeMap<i64, Val>),
    MapStrF64(BTreeMap<String, f64>),
    SetStr(BTreeSet<String>),
    SetI64(BTreeSet<i64>),
    Boxed(Box<Val>),
    UnitStruct(UnitStruct),
    Empty(Empty),
    Point(Point),
    Pair(Box<Pair>),
    Wrap(Box<Wrap>),
    SessionId(SessionId),
    Meters(Meters),
    Count(Count),
    Tags(Tags),
    Kind(Kind),
    State(Box<State>),
    Record(Box<Record>),
}

impl Debug for Val {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Val::I8(v) => Debug::fmt(v, f),
            Val::I16(v) => Debug::fmt(v, f),
            Val::I32(v) => Debug::fmt(v, f),
            Val::I64(v) => Debug::fmt(v, f),
            Val::I128(v) => Debug::fmt(v, f),
            Val::Isize(v) => Debug::fmt(v, f),
            Val::U8(v) => Debug::fmt(v, f),
            Val::U16(v) => Debug::fmt(v, f),
            Val::U32(v) => Debug::fmt(v, f),
            Val::U64(v) => Debug::fmt(v, f),
            Val::U128(v) => Debug::fmt(v, f),
            Val::Usize(v) => Debug::fmt(v, f),
            Val::F32(v) => Debug::fmt(v, f),
            Val::F64(v) => Debug::fmt(v, f),
            Val::Bool(v) => Debug::fmt(v, f),
            Val::Char(v) => Debug::fmt(v, f),
            Val::Str(v) => Debug::fmt(v, f),
            Val::Unit => Debug::fmt(&(), f),
            Val::Dur(v) => Debug::fmt(v, f),
            Val::Opt(v) => Debug::fmt(v, f),
            Val::Res(v) => Debug::fmt(v, f),
            Val::Vec(v) => Debug::fmt(v, f),
            Val::Deque(v) => Debug::fmt(v, f),
            Val::Bytes(v) => Debug::fmt(v, f),
            Val::VecI32(v) => Debug::fmt(v, f),
            Val::VecStr(v) => Debug::fmt(v, f),
            Val::Arr3(v) => Debug::fmt(v, f),
            Val::T2(v) => Debug::fmt(v, f),
            Val::T3(v) => Debug::fmt(v, f),
            Val::T4(v) => Debug::fmt(v, f),
            Val::T5(v) => Debug::fmt(v, f),
            Val::MapStr(v) => Debug::fmt(v, f),
            Val::MapI64(v) => Debug::fmt(v, f),
            Val::MapStrF64(v) => Debug::fmt(v, f),
            Val::SetStr(v) => Debug::fmt(v, f),
            Val::SetI64(v) => Debug::fmt(v, f),
            Val::Boxed(v) => Debug::fmt(v, f),
            Val::UnitStruct(v) => Debug::fmt(v, f),
            Val::Empty(v) => Debug::fmt(v, f),
            Val::Point(v) => Debug::fmt(v, f),
            Val::Pair(v) => Debug::fmt(v, f),
            Val::Wrap(v) => Debug::fmt(v, f),
            Val::SessionId(v) => Debug::fmt(v, f),
            Val::Meters(v) => Debug::fmt(v, f),
            Val::Count(v) => Debug::fmt(v, f),
            Val::Tags(v) => Debug::fmt(v, f),
            Val::Kind(v) => Debug::fmt(v, f),
            Val::State(v) => Debug::fmt(v, f),
            Val::Record(v) => Debug::fmt(v, f),
        }
    }
}

macro_rules! integers {
    ($self:ident, $tr:path) => {
        match $self {
            Val::I8(v) => Some(v as &dyn $tr),
            Val::I16(v) => Some(v as &dyn $tr),
            Val::I32(v) => Some(v as &dyn $tr),
            Val::I64(v) => Some(v as &dyn $tr),
            Val::I128(v) => Some(v as &dyn $tr),
            Val::Isize(v) => Some(v as &dyn $tr),
            Val::U8(v) => Some(v as &dyn $tr),
            Val::U16(v) => Some(v as &dyn $tr),
            Val::U32(v) => Some(v as &dyn $tr),
            Val::U64(v) => Some(v as &dyn $tr),
            Val::U128(v) => Some(v as &dyn $tr),
            Val::Usize(v) => Some(v as &dyn $tr),
            _ => None,
        }
    };
}

impl Val {
    fn display(&self) -> Option<&dyn Display> {
        match self {
            Val::F32(v) => Some(v),
            Val::F64(v) => Some(v),
            Val::Bool(v) => Some(v),
            Val::Char(v) => Some(v),
            Val::Str(v) => Some(v),
            Val::Boxed(v) => v.display(),
            _ => integers!(self, Display),
        }
    }

    fn lower_hex(&self) -> Option<&dyn LowerHex> {
        integers!(self, LowerHex)
    }

    fn upper_hex(&self) -> Option<&dyn UpperHex> {
        integers!(self, UpperHex)
    }

    fn octal(&self) -> Option<&dyn Octal> {
        integers!(self, Octal)
    }

    fn binary(&self) -> Option<&dyn Binary> {
        integers!(self, Binary)
    }

    fn lower_exp(&self) -> Option<&dyn LowerExp> {
        match self {
            Val::F32(v) => Some(v),
            Val::F64(v) => Some(v),
            _ => integers!(self, LowerExp),
        }
    }

    fn upper_exp(&self) -> Option<&dyn UpperExp> {
        match self {
            Val::F32(v) => Some(v),
            Val::F64(v) => Some(v),
            _ => integers!(self, UpperExp),
        }
    }
}

fn text(v: &Value) -> &str {
    v.as_str().unwrap_or_else(|| panic!("oracle: expected a string, got {v}"))
}

fn number<T: std::str::FromStr>(v: &Value) -> T
where
    T::Err: Debug,
{
    text(v).parse().unwrap_or_else(|error| panic!("oracle: bad number {v}: {error:?}"))
}

fn items(v: &Value) -> &[Value] {
    v.as_array().unwrap_or_else(|| panic!("oracle: expected an array, got {v}"))
}

fn duration(secs: &Value, nanos: &Value) -> Duration {
    Duration::new(number(secs), nanos.as_u64().expect("nanos") as u32)
}

/// Builds the value a request describes: `[TAG, ..]`. Integers and the bits
/// of floats are decimal strings, so that 64 and 128 bits survive JSON.
fn value(v: &Value) -> Val {
    let parts = items(v);
    let tag = text(&parts[0]);
    match tag {
        "i8" => Val::I8(number(&parts[1])),
        "i16" => Val::I16(number(&parts[1])),
        "i32" => Val::I32(number(&parts[1])),
        "i64" => Val::I64(number(&parts[1])),
        "i128" => Val::I128(number(&parts[1])),
        "isize" => Val::Isize(number(&parts[1])),
        "u8" => Val::U8(number(&parts[1])),
        "u16" => Val::U16(number(&parts[1])),
        "u32" => Val::U32(number(&parts[1])),
        "u64" => Val::U64(number(&parts[1])),
        "u128" => Val::U128(number(&parts[1])),
        "usize" => Val::Usize(number(&parts[1])),
        "f32" => Val::F32(f32::from_bits(number(&parts[1]))),
        "f64" => Val::F64(f64::from_bits(number(&parts[1]))),
        "bool" => Val::Bool(parts[1].as_bool().expect("bool")),
        "char" => Val::Char(char::from_u32(parts[1].as_u64().expect("char") as u32).expect("char")),
        "str" => Val::Str(text(&parts[1]).to_owned()),
        "unit" => Val::Unit,
        "dur" => Val::Dur(duration(&parts[1], &parts[2])),
        "some" => Val::Opt(Some(Box::new(value(&parts[1])))),
        "none" => Val::Opt(None),
        "ok" => Val::Res(Ok(Box::new(value(&parts[1])))),
        "err" => Val::Res(Err(ParseFailure {
            line: parts[1].as_u64().expect("line") as u32,
            reason: text(&parts[2]).to_owned(),
        })),
        "vec" => Val::Vec(items(&parts[1]).iter().map(value).collect()),
        "deque" => Val::Deque(items(&parts[1]).iter().map(value).collect()),
        "bytes" => Val::Bytes(items(&parts[1]).iter().map(|b| b.as_u64().expect("byte") as u8).collect()),
        "vec_i32" => Val::VecI32(items(&parts[1]).iter().map(|n| n.as_i64().expect("i32") as i32).collect()),
        "vec_str" => Val::VecStr(items(&parts[1]).iter().map(|s| text(s).to_owned()).collect()),
        "arr3" => {
            let n: Vec<i32> = items(&parts[1]).iter().map(|n| n.as_i64().expect("i32") as i32).collect();
            Val::Arr3([n[0], n[1], n[2]])
        }
        "t2" => Val::T2(Box::new((value(&parts[1]), value(&parts[2])))),
        "t3" => Val::T3(Box::new((value(&parts[1]), value(&parts[2]), value(&parts[3])))),
        "t4" => Val::T4(Box::new((value(&parts[1]), value(&parts[2]), value(&parts[3]), value(&parts[4])))),
        "t5" => Val::T5(Box::new((
            value(&parts[1]),
            value(&parts[2]),
            value(&parts[3]),
            value(&parts[4]),
            value(&parts[5]),
        ))),
        "map_str" => {
            Val::MapStr(items(&parts[1]).iter().map(|e| (text(&e[0]).to_owned(), value(&e[1]))).collect())
        }
        "map_i64" => Val::MapI64(items(&parts[1]).iter().map(|e| (number(&e[0]), value(&e[1]))).collect()),
        "map_str_f64" => Val::MapStrF64(
            items(&parts[1]).iter().map(|e| (text(&e[0]).to_owned(), f64::from_bits(number(&e[1])))).collect(),
        ),
        "set_str" => Val::SetStr(items(&parts[1]).iter().map(|s| text(s).to_owned()).collect()),
        "set_i64" => Val::SetI64(items(&parts[1]).iter().map(number).collect()),
        "box" => Val::Boxed(Box::new(value(&parts[1]))),
        "UnitStruct" => Val::UnitStruct(UnitStruct),
        "Empty" => Val::Empty(Empty {}),
        "Point" => Val::Point(Point {
            x: parts[1].as_i64().expect("x") as i32,
            y: parts[2].as_i64().expect("y") as i32,
        }),
        "Pair" => Val::Pair(Box::new(Pair(value(&parts[1]), value(&parts[2])))),
        "Wrap" => Val::Wrap(Box::new(Wrap(value(&parts[1])))),
        "SessionId" => Val::SessionId(SessionId(text(&parts[1]).to_owned())),
        "Meters" => Val::Meters(Meters(f64::from_bits(number(&parts[1])))),
        "Count" => Val::Count(Count(parts[1].as_u64().expect("count") as u32)),
        "Tags" => Val::Tags(Tags(items(&parts[1]).iter().map(|s| text(s).to_owned()).collect())),
        "Kind" => Val::Kind(kind(&parts[1])),
        "State" => Val::State(Box::new(state(&parts[1]))),
        "Record" => Val::Record(Box::new(record(&parts[1]))),
        _ => panic!("oracle: no value tag `{tag}`"),
    }
}

fn kind(v: &Value) -> Kind {
    match v.as_u64().expect("kind") {
        0 => Kind::Alpha,
        1 => Kind::Beta,
        _ => Kind::GammaDelta,
    }
}

/// `[VARIANT, fields..]`
fn state(v: &Value) -> State {
    let parts = items(v);
    match text(&parts[0]) {
        "Idle" => State::Idle,
        "Running" => State::Running {
            pid: parts[1].as_u64().expect("pid") as u32,
            since: duration(&parts[2], &parts[3]),
        },
        "Failed" => State::Failed(text(&parts[1]).to_owned(), parts[2].as_i64().expect("code") as i32),
        "Wrapped" => State::Wrapped(Box::new(state(&parts[1]))),
        "Maybe" => State::Maybe(parts[1].as_u64().map(|n| n as u8)),
        "Payload" => State::Payload(value(&parts[1])),
        other => panic!("oracle: no State variant `{other}`"),
    }
}

/// The fields of a Record as an object.
fn record(v: &Value) -> Record {
    Record {
        id: number(&v["id"]),
        name: text(&v["name"]).to_owned(),
        tags: items(&v["tags"]).iter().map(|s| text(s).to_owned()).collect(),
        parent: if v["parent"].is_null() { None } else { Some(Box::new(record(&v["parent"]))) },
        scores: items(&v["scores"])
            .iter()
            .map(|e| (text(&e[0]).to_owned(), f64::from_bits(number(&e[1]))))
            .collect(),
        kind: kind(&v["kind"]),
        state: state(&v["state"]),
        raw: items(&v["raw"]).iter().map(|b| b.as_u64().expect("byte") as u8).collect(),
        elapsed: duration(&v["elapsed"][0], &v["elapsed"][1]),
        pair: (v["pair"][0].as_i64().expect("pair") as i32, text(&v["pair"][1]).to_owned()),
        ch: char::from_u32(v["ch"].as_u64().expect("ch") as u32).expect("ch"),
        unit: (),
        flag: v["flag"].as_bool().expect("flag"),
        ratio: f32::from_bits(number(&v["ratio"])),
        session: SessionId(text(&v["session"]).to_owned()),
        private_note: v["private_note"].as_str().map(str::to_owned),
        any: value(&v["any"]),
    }
}

// Whole format strings: what the Go parser must read as rustc does.

struct Shown(&'static str);

impl Display for Shown {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            f,
            "<{} fill={:?} align={:?} width={:?} precision={:?} plus={} minus={} alt={} zero={}>",
            self.0,
            f.fill(),
            f.align(),
            f.width(),
            f.precision(),
            f.sign_plus(),
            f.sign_minus(),
            f.alternate(),
            f.sign_aware_zero_pad()
        )
    }
}

impl Debug for Shown {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let width = f.width();
        f.debug_struct("Shown").field("name", &self.0).field("width", &width).finish_non_exhaustive()
    }
}

/// The fixed format strings and their arguments. The Go test passes the same
/// arguments under the same names (`fmtOraclePool`), and names the variables a
/// format string captures from this scope.
#[allow(named_arguments_used_positionally)]
fn template(format: &str, args: &str) -> Option<String> {
    let a = 13i32;
    let b = 14u8;
    let neg = -7i64;
    let s = "héllo";
    let x = 1.5f64;
    let y = -0.25f32;
    let w = 7usize;
    let p = 2usize;
    let c = 'z';
    let name = "World";
    let list = vec![1i32, 2, 3];
    let opt = Some("x");
    let shown = Shown("shown");
    macro_rules! templates {
        ($($literal:literal => ($($args:tt)*),)*) => {
            match (format, args) {
                $(($literal, stringify!($($args)*)) => Some(format!($literal, $($args)*)),)*
                _ => None,
            }
        };
    }
    templates! {
        "" => (),
        "plain text" => (),
        "{{}}" => (),
        "{{{}}}" => (a),
        "}}{{" => (),
        "a{{b}}c{}d" => (a),
        "{}" => (a),
        "{} {}" => (a, b),
        "{0} {0}" => (a),
        "{1} {0}" => (a, b),
        "{} {0} {}" => (a, b),
        "{0} {} {1} {}" => (a, b),
        "{name}" => (),
        "{name} {}" => (a),
        "{} {name} {}" => (a, b),
        "{name}{name}" => (),
        "{n}" => (n = a),
        "{} {n} {0}" => (b, n = a),
        "{0} {1}" => (b, n = a),
        "{} {}" => (b, n = a),
        "{ }" => (a),
        "{0 }" => (a),
        "{name }" => (),
        "{:x }" => (a),
        "{0:x }" => (a),
        "{:5}|" => (a),
        "{:<5}|" => (a),
        "{:^5}|" => (a),
        "{:>5}|" => (a),
        "{:*<5}|" => (a),
        "{:}<5}|" => (a),
        "{:{<5}|" => (a),
        "{:>>5}|" => (a),
        "{:<<5}|" => (a),
        "{:^^5}|" => (a),
        "{:é^6}|" => (a),
        "{:🎉>4}|" => (a),
        "{:0<5}|" => (a),
        "{:+}" => (a),
        "{:+}" => (neg),
        "{:-}" => (a),
        "{:#x}" => (a),
        "{:#X}" => (a),
        "{:#o}" => (a),
        "{:#b}" => (a),
        "{:05}" => (neg),
        "{:+05}" => (a),
        "{:#06x}" => (a),
        "{:<#06x}|" => (a),
        "{:0}" => (a),
        "{:00}" => (a),
        "{:01}" => (a),
        "{:010}" => (a),
        "{:w$}|" => (a),
        "{:1$}|" => (a, w),
        "{:0$}|" => (w),
        "{0:1$}|" => (a, w),
        "{:w$.p$}|" => (x),
        "{:1$.2$}|" => (x, w, p),
        "{:.*}|" => (p, x),
        "{:.*} {}" => (p, x, a),
        "{} {:.*} {}" => (a, p, x, b),
        "{1:.*}" => (p, x),
        "{0:.*}" => (w),
        "{:.*} {:.*}" => (p, x, w, x),
        "{:w$.*}|" => (p, x),
        "{name:.*}|" => (p),
        "{:.p$}|" => (s),
        "{:.0}|" => (s),
        "{:.3}|" => (s),
        "{:8.3}|" => (s),
        "{:>8.3}|" => (s),
        "{:^w$.p$}|" => (s),
        "{:?}" => (s),
        "{:#?}" => (list),
        "{:x?}" => (list),
        "{:X?}" => (list),
        "{:#x?}" => (list),
        "{:02x?}" => (list),
        "{:#04X?}" => (list),
        "{:?} {:?}" => (opt, c),
        "{:e}" => (x),
        "{:E}" => (x),
        "{:e}" => (a),
        "{:.1e}" => (y),
        "{:10.3e}|" => (x),
        "{:b}" => (b),
        "{:o}" => (b),
        "{}{}{}{}" => (a, b, c, s),
        "{3}{2}{1}{0}" => (a, b, c, s),
        "{s} {a:>w$} {x:.p$}" => (),
        "{a}{b}{c}" => (),
        "{:>1$} {:>1$}" => (a, w),
        "{0:>2$} {1:>2$}" => (a, b, w),
        "{:width$}|" => (a, width = w),
        "{:.prec$}|" => (x, prec = p),
        "{:width$.prec$}|" => (x, width = w, prec = p),
        "{x:w$.p$}|{y:w$.p$}|" => (),
        "{_x}" => (_x = a),
        "{x1} {x_2}" => (x1 = a, x_2 = b),
        "{é}" => (é = a),
        "{shown}" => (),
        "{shown:>10.3}" => (),
        "{shown:*^+#012.4}" => (),
        "{shown:<-w$.p$}" => (),
        "{shown:?}" => (),
        "{shown:9?}" => (),
        "{shown:#?}" => (),
        "{shown:.*}" => (p),
        "{shown:0$}" => (w),
        "tab\there\nnewline {} \"quoted\" \\ end" => (a),
        "unicode ✓ {} ☃" => (s),
    }
}
