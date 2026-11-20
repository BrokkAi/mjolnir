//! Oracle module `std_cmp`: what `#[derive(Clone, PartialEq, PartialOrd, Ord,
//! Default)]`, `core::cmp::Ordering`, `mem::discriminant`, `Arc` and `Weak`
//! do, for the structural operations of the Go port in `go/rs` (`clone.go`,
//! `eq.go`, `cmp.go`, `default.go`, `arc.go`).
//!
//! The Go side is `go/rs/cmp_oracle_test.go` and `go/rs/arc_oracle_test.go`
//! (package `rs_test`); its types mirror the ones declared here.
//!
//! Requests:
//!
//! - `{"op":"cmp","a":VAL,"b":VAL}` replies `{"eq":..,"partial_cmp":ORD,
//!   "clone_eq":..,"same_variant":..}`: `a == b`, `a.partial_cmp(&b)`,
//!   `a.clone() == a` and `discriminant(&a) == discriminant(&b)`.
//! - `{"op":"ord","a":KEY,"b":KEY}` replies `{"eq":..,"cmp":ORD,"min":KEY,
//!   "max":KEY}` for two `Key`s, which are `Ord`.
//! - `{"op":"hash","a":[[KEY,VAL],..],"b":..,"sa":[KEY,..],"sb":..}` replies
//!   `{"map_eq":..,"set_eq":..}`: whether the `HashMap<Key, Val>`s built by
//!   inserting the pairs in order are equal, and the `HashSet<Key>`s.
//! - `{"op":"ordering","a":ORD,"b":ORD}` replies with what the methods of
//!   `Ordering` give for the two.
//! - `{"op":"default","type":NAME}` replies `{"value":DESCRIPTION}` for
//!   `NAME::default()`; `default_value` lists the names and `Describe` the
//!   description.
//! - `{"op":"clone","n":N}` clones a `Holder` whose lists hold N elements and
//!   replies with what the clone copied, what it shared, and what dropping
//!   the clone drops.
//! - `{"op":"arc","steps":[STEP,..]}` runs a script on `Arc<Tracked>` and
//!   `Weak<Tracked>` handles and replies `{"steps":[[RESULT,[DROPPED,..]],..],
//!   "final":[DROPPED,..]}`: what each step observed and the values it
//!   dropped, then the values dropped when the remaining handles go;
//!   `arc_script` lists the steps.
//!
//! VAL is `[TAG, ..]`, the name of a variant of `Val` and its payload; KEY
//! likewise for `Key`. Floats are written as text (`"1.5"`, `"-0"`, `"NaN"`,
//! `"-Inf"`) and read with `str::parse`. ORD is -1, 0 or 1 for `Less`,
//! `Equal` and `Greater`, and null for `None`.

use std::cell::{Cell, RefCell};
use std::cmp::{Ordering, Reverse};
use std::collections::{BTreeMap, BTreeSet, HashMap, HashSet, VecDeque};
use std::fmt::Debug;
use std::str::FromStr;
use std::sync::{Arc, Weak};

use serde_json::{Value, json};

pub fn handle(request: &Value) -> Value {
    match crate::text(request, "op") {
        "cmp" => {
            let a = val(&request["a"]);
            let b = val(&request["b"]);
            json!({
                "eq": a == b,
                "partial_cmp": ord(a.partial_cmp(&b)),
                "clone_eq": a.clone() == a,
                "same_variant": std::mem::discriminant(&a) == std::mem::discriminant(&b),
            })
        }
        "ord" => {
            let a = key(&request["a"]);
            let b = key(&request["b"]);
            json!({
                "eq": a == b,
                "cmp": ord(Some(a.cmp(&b))),
                "min": key_json(&std::cmp::min(a.clone(), b.clone())),
                "max": key_json(&std::cmp::max(a, b)),
            })
        }
        "hash" => {
            let map = |pairs: &Value| {
                let mut map = HashMap::new();
                for pair in parts(pairs) {
                    let pair = parts(pair);
                    map.insert(key(&pair[0]), val(&pair[1]));
                }
                map
            };
            let set = |keys: &Value| {
                let mut set = HashSet::new();
                for item in parts(keys) {
                    set.insert(key(item));
                }
                set
            };
            json!({
                "map_eq": map(&request["a"]) == map(&request["b"]),
                "set_eq": set(&request["sa"]) == set(&request["sb"]),
            })
        }
        "ordering" => {
            let a = ordering(&request["a"]);
            let b = ordering(&request["b"]);
            json!({
                "reverse": ord(Some(a.reverse())),
                "then": ord(Some(a.then(b))),
                "then_with": ord(Some(a.then_with(|| b))),
                "is_eq": a.is_eq(),
                "is_ne": a.is_ne(),
                "is_lt": a.is_lt(),
                "is_gt": a.is_gt(),
                "is_le": a.is_le(),
                "is_ge": a.is_ge(),
                "eq": a == b,
                "cmp": ord(Some(a.cmp(&b))),
                "partial_cmp": ord(a.partial_cmp(&b)),
                "min": ord(Some(a.min(b))),
                "max": ord(Some(a.max(b))),
                "as_i8": a as i8,
                "debug": format!("{a:?}"),
            })
        }
        "default" => match default_value(crate::text(request, "type")) {
            Some(value) => json!({ "value": value }),
            None => crate::unknown(request),
        },
        "clone" => clone_holder(request["n"].as_u64().expect("n") as usize),
        "arc" => arc_script(request["steps"].as_array().expect("steps")),
        _ => crate::unknown(request),
    }
}

fn ord(order: Option<Ordering>) -> Value {
    match order {
        Some(Ordering::Less) => json!(-1),
        Some(Ordering::Equal) => json!(0),
        Some(Ordering::Greater) => json!(1),
        None => Value::Null,
    }
}

fn ordering(value: &Value) -> Ordering {
    match value.as_i64().expect("an ordering") {
        -1 => Ordering::Less,
        0 => Ordering::Equal,
        1 => Ordering::Greater,
        other => panic!("not an ordering: {other}"),
    }
}

// Reading requests.

fn parts(value: &Value) -> &[Value] {
    value.as_array().map(Vec::as_slice).expect("an array")
}

fn int<T>(value: &Value) -> T
where
    T: TryFrom<i128>,
    T::Error: Debug,
{
    let wide = match value.as_i64() {
        Some(n) => i128::from(n),
        None => i128::from(value.as_u64().expect("an integer")),
    };
    T::try_from(wide).expect("an integer in range")
}

fn float<T>(value: &Value) -> T
where
    T: FromStr,
    T::Err: Debug,
{
    value.as_str().expect("a float as text").parse().expect("a float")
}

fn boolean(value: &Value) -> bool {
    value.as_bool().expect("a bool")
}

fn string(value: &Value) -> String {
    value.as_str().expect("a string").to_owned()
}

fn chr(value: &Value) -> char {
    let text = value.as_str().expect("a char");
    let mut chars = text.chars();
    let c = chars.next().expect("one char");
    assert!(chars.next().is_none(), "one char, not {text:?}");
    c
}

fn list<T>(value: &Value, item: impl Fn(&Value) -> T) -> Vec<T> {
    parts(value).iter().map(item).collect()
}

fn option<T>(value: &Value, some: impl Fn(&Value) -> T) -> Option<T> {
    if value.is_null() { None } else { Some(some(value)) }
}

// The values compared.

/// Every shape of value the Go port compares by reflection, as the payload of
/// one variant each: an enum ordered by variant, then by fields.
#[derive(Clone, PartialEq, PartialOrd)]
enum Val {
    Unit,
    Bool(bool),
    I8(i8),
    I16(i16),
    I32(i32),
    I64(i64),
    Isize(isize),
    U8(u8),
    U16(u16),
    U32(u32),
    U64(u64),
    Usize(usize),
    F32(f32),
    F64(f64),
    Char(char),
    Str(String),
    Bytes(Vec<u8>),
    Ints(Vec<i64>),
    Floats(Vec<f64>),
    Strs(Vec<String>),
    Array([i16; 3]),
    Grid([[bool; 2]; 2]),
    MaybeInt(Option<i32>),
    MaybeFloat(Option<f64>),
    MaybeVal(Option<Box<Val>>),
    List(Vec<Val>),
    Pair((i32, String)),
    Triple(Box<Val>, f64, bool),
    Rec {
        id: u32,
        name: String,
        score: f64,
        tags: Vec<String>,
        child: Option<Box<Val>>,
    },
    Point(Point),
    Color(Color),
    Res(Result<i32, String>),
    Rev(Reverse<i32>),
    RevF(Reverse<f64>),
    Map(BTreeMap<Key, Val>),
    Set(BTreeSet<Key>),
    Deque(VecDeque<i8>),
    Boxed(Box<Val>),
    Key(Key),
    Cell(Cell<i32>),
    RefCell(RefCell<Vec<i32>>),
    Counted(Arc<f64>),
    Shared(Arc<Point>),
    Void(()),
}

#[derive(Clone, PartialEq, Eq, PartialOrd, Ord, Hash)]
struct Point {
    x: i32,
    y: i32,
    label: String,
}

/// A data-free enum whose discriminants are not in declaration order: it is
/// ordered by discriminant.
#[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
enum Color {
    Red = 2,
    Green = 0,
    Blue = 1,
}

/// Map and set keys: a totally ordered, hashable enum.
#[derive(Clone, PartialEq, Eq, PartialOrd, Ord, Hash)]
enum Key {
    Empty,
    Num(i64),
    Text(String),
    Pair(u8, bool),
    Nested(Vec<Key>),
    Tagged { tag: char, inner: Option<Box<Key>> },
}

fn point(value: &Value) -> Point {
    Point { x: int(&value["x"]), y: int(&value["y"]), label: string(&value["label"]) }
}

fn color(value: &Value) -> Color {
    match value.as_str().expect("a color") {
        "Red" => Color::Red,
        "Green" => Color::Green,
        "Blue" => Color::Blue,
        other => panic!("not a color: {other}"),
    }
}

fn val(value: &Value) -> Val {
    let p = parts(value);
    match p[0].as_str().expect("a tag") {
        "Unit" => Val::Unit,
        "Bool" => Val::Bool(boolean(&p[1])),
        "I8" => Val::I8(int(&p[1])),
        "I16" => Val::I16(int(&p[1])),
        "I32" => Val::I32(int(&p[1])),
        "I64" => Val::I64(int(&p[1])),
        "Isize" => Val::Isize(int(&p[1])),
        "U8" => Val::U8(int(&p[1])),
        "U16" => Val::U16(int(&p[1])),
        "U32" => Val::U32(int(&p[1])),
        "U64" => Val::U64(int(&p[1])),
        "Usize" => Val::Usize(int(&p[1])),
        "F32" => Val::F32(float(&p[1])),
        "F64" => Val::F64(float(&p[1])),
        "Char" => Val::Char(chr(&p[1])),
        "Str" => Val::Str(string(&p[1])),
        "Bytes" => Val::Bytes(list(&p[1], int)),
        "Ints" => Val::Ints(list(&p[1], int)),
        "Floats" => Val::Floats(list(&p[1], float)),
        "Strs" => Val::Strs(list(&p[1], string)),
        "Array" => {
            let items: Vec<i16> = list(&p[1], int);
            Val::Array(items.try_into().expect("three items"))
        }
        "Grid" => {
            let rows: Vec<[bool; 2]> = list(&p[1], |row| {
                let cells: Vec<bool> = list(row, boolean);
                cells.try_into().expect("two cells")
            });
            Val::Grid(rows.try_into().expect("two rows"))
        }
        "MaybeInt" => Val::MaybeInt(option(&p[1], int)),
        "MaybeFloat" => Val::MaybeFloat(option(&p[1], float)),
        "MaybeVal" => Val::MaybeVal(option(&p[1], |v| Box::new(val(v)))),
        "List" => Val::List(list(&p[1], val)),
        "Pair" => Val::Pair((int(&p[1]), string(&p[2]))),
        "Triple" => Val::Triple(Box::new(val(&p[1])), float(&p[2]), boolean(&p[3])),
        "Rec" => {
            let r = &p[1];
            Val::Rec {
                id: int(&r["id"]),
                name: string(&r["name"]),
                score: float(&r["score"]),
                tags: list(&r["tags"], string),
                child: option(&r["child"], |v| Box::new(val(v))),
            }
        }
        "Point" => Val::Point(point(&p[1])),
        "Color" => Val::Color(color(&p[1])),
        "Res" => Val::Res(match p[1].get("ok") {
            Some(n) => Ok(int(n)),
            None => Err(string(&p[1]["err"])),
        }),
        "Rev" => Val::Rev(Reverse(int(&p[1]))),
        "RevF" => Val::RevF(Reverse(float(&p[1]))),
        "Map" => {
            let mut map = BTreeMap::new();
            for pair in parts(&p[1]) {
                let pair = parts(pair);
                map.insert(key(&pair[0]), val(&pair[1]));
            }
            Val::Map(map)
        }
        "Set" => {
            let mut set = BTreeSet::new();
            for item in parts(&p[1]) {
                set.insert(key(item));
            }
            Val::Set(set)
        }
        "Deque" => {
            let items: Vec<i8> = list(&p[1], int);
            Val::Deque(VecDeque::from(items))
        }
        "Boxed" => Val::Boxed(Box::new(val(&p[1]))),
        "Key" => Val::Key(key(&p[1])),
        "Cell" => Val::Cell(Cell::new(int(&p[1]))),
        "RefCell" => Val::RefCell(RefCell::new(list(&p[1], int))),
        "Counted" => Val::Counted(Arc::new(float(&p[1]))),
        "Shared" => Val::Shared(Arc::new(point(&p[1]))),
        "Void" => Val::Void(()),
        other => panic!("not a Val tag: {other}"),
    }
}

fn key(value: &Value) -> Key {
    let p = parts(value);
    match p[0].as_str().expect("a tag") {
        "Empty" => Key::Empty,
        "Num" => Key::Num(int(&p[1])),
        "Text" => Key::Text(string(&p[1])),
        "Pair" => Key::Pair(int(&p[1]), boolean(&p[2])),
        "Nested" => Key::Nested(list(&p[1], key)),
        "Tagged" => Key::Tagged {
            tag: chr(&p[1]["tag"]),
            inner: option(&p[1]["inner"], |v| Box::new(key(v))),
        },
        other => panic!("not a Key tag: {other}"),
    }
}

fn key_json(key: &Key) -> Value {
    match key {
        Key::Empty => json!(["Empty"]),
        Key::Num(n) => json!(["Num", n]),
        Key::Text(text) => json!(["Text", text]),
        Key::Pair(n, flag) => json!(["Pair", n, flag]),
        Key::Nested(keys) => json!(["Nested", keys.iter().map(key_json).collect::<Vec<_>>()]),
        Key::Tagged { tag, inner } => {
            json!(["Tagged", { "tag": tag.to_string(), "inner": inner.as_deref().map(key_json) }])
        }
    }
}

// Default.

/// A description of a value that the Go side can produce too: structs as
/// objects by field name, enums by variant name, floats by their bits.
trait Describe {
    fn describe(&self) -> Value;
}

macro_rules! describe_as_json {
    ($($t:ty),*) => {$(
        impl Describe for $t {
            fn describe(&self) -> Value {
                json!(self)
            }
        }
    )*};
}

describe_as_json!(bool, u8, u32, u64, i32, i64, String);

impl Describe for f64 {
    fn describe(&self) -> Value {
        json!(self.to_bits())
    }
}

impl Describe for char {
    fn describe(&self) -> Value {
        json!(self.to_string())
    }
}

impl Describe for () {
    fn describe(&self) -> Value {
        json!("()")
    }
}

impl<T: Describe> Describe for Box<T> {
    fn describe(&self) -> Value {
        (**self).describe()
    }
}

impl<T: Describe> Describe for Arc<T> {
    fn describe(&self) -> Value {
        (**self).describe()
    }
}

impl<T: Describe> Describe for Option<T> {
    fn describe(&self) -> Value {
        match self {
            Some(value) => json!({ "Some": value.describe() }),
            None => json!("None"),
        }
    }
}

impl<T: Describe> Describe for Vec<T> {
    fn describe(&self) -> Value {
        Value::Array(self.iter().map(Describe::describe).collect())
    }
}

impl<T: Describe, const N: usize> Describe for [T; N] {
    fn describe(&self) -> Value {
        Value::Array(self.iter().map(Describe::describe).collect())
    }
}

impl<A: Describe, B: Describe> Describe for (A, B) {
    fn describe(&self) -> Value {
        json!([self.0.describe(), self.1.describe()])
    }
}

impl<V: Describe> Describe for HashMap<String, V> {
    fn describe(&self) -> Value {
        Value::Object(self.iter().map(|(k, v)| (k.clone(), v.describe())).collect())
    }
}

impl<V: Describe> Describe for BTreeMap<String, V> {
    fn describe(&self) -> Value {
        Value::Object(self.iter().map(|(k, v)| (k.clone(), v.describe())).collect())
    }
}

impl<T: Describe + Copy> Describe for Cell<T> {
    fn describe(&self) -> Value {
        self.get().describe()
    }
}

impl<T: Describe> Describe for RefCell<T> {
    fn describe(&self) -> Value {
        self.borrow().describe()
    }
}

impl<T: Describe> Describe for Reverse<T> {
    fn describe(&self) -> Value {
        self.0.describe()
    }
}

/// A data-free enum whose `#[default]` is not its first variant. Only its
/// default is ever made.
#[allow(dead_code)]
#[derive(Clone, Copy, Debug, Default, PartialEq)]
enum Mode {
    Off,
    #[default]
    Push,
    Always,
}

impl Describe for Mode {
    fn describe(&self) -> Value {
        json!(format!("{self:?}"))
    }
}

/// An enum with data whose `#[default]` is a unit variant.
#[allow(dead_code)]
#[derive(Clone, Debug, Default)]
enum Layout {
    #[default]
    Stacked,
    Split { ratio: f64 },
}

impl Describe for Layout {
    fn describe(&self) -> Value {
        match self {
            Layout::Stacked => json!("Stacked"),
            Layout::Split { ratio } => json!({ "Split": { "ratio": ratio.describe() } }),
        }
    }
}

/// A struct with a hand-written Default.
#[derive(Clone, Debug)]
struct Limit {
    max: u32,
    hosts: Vec<String>,
}

impl Default for Limit {
    fn default() -> Self {
        Limit { max: 10, hosts: vec!["localhost".to_owned()] }
    }
}

impl Describe for Limit {
    fn describe(&self) -> Value {
        json!({ "max": self.max.describe(), "hosts": self.hosts.describe() })
    }
}

#[derive(Clone, Debug, Default)]
struct Leaf {
    mode: Mode,
    name: String,
}

impl Describe for Leaf {
    fn describe(&self) -> Value {
        json!({ "mode": self.mode.describe(), "name": self.name.describe() })
    }
}

/// A struct with `#[derive(Default)]` holding every kind of field.
#[derive(Debug, Default)]
struct Settings {
    mode: Mode,
    layout: Layout,
    limit: Limit,
    boxed: Box<Leaf>,
    shared: Arc<Leaf>,
    counted: Arc<Leaf>,
    maybe: Option<Box<Leaf>>,
    names: Vec<String>,
    lookup: HashMap<String, i32>,
    tree: BTreeMap<String, Mode>,
    pair: (Mode, Box<Limit>),
    modes: [Mode; 2],
    scroll: Cell<Mode>,
    areas: RefCell<Vec<Mode>>,
    rev: Reverse<Mode>,
    count: u64,
    ratio: f64,
    flag: bool,
    letter: char,
    title: String,
    unit: (),
    nested: Option<Limit>,
}

impl Describe for Settings {
    fn describe(&self) -> Value {
        json!({
            "mode": self.mode.describe(),
            "layout": self.layout.describe(),
            "limit": self.limit.describe(),
            "boxed": self.boxed.describe(),
            "shared": self.shared.describe(),
            "counted": self.counted.describe(),
            "maybe": self.maybe.describe(),
            "names": self.names.describe(),
            "lookup": self.lookup.describe(),
            "tree": self.tree.describe(),
            "pair": self.pair.describe(),
            "modes": self.modes.describe(),
            "scroll": self.scroll.describe(),
            "areas": self.areas.describe(),
            "rev": self.rev.describe(),
            "count": self.count.describe(),
            "ratio": self.ratio.describe(),
            "flag": self.flag.describe(),
            "letter": self.letter.describe(),
            "title": self.title.describe(),
            "unit": self.unit.describe(),
            "nested": self.nested.describe(),
        })
    }
}

fn describe_default<T: Default + Describe>() -> Value {
    T::default().describe()
}

fn default_value(name: &str) -> Option<Value> {
    Some(match name {
        "Settings" => describe_default::<Settings>(),
        "Mode" => describe_default::<Mode>(),
        "Layout" => describe_default::<Layout>(),
        "Limit" => describe_default::<Limit>(),
        "Leaf" => describe_default::<Leaf>(),
        "Option<Box<Leaf>>" => describe_default::<Option<Box<Leaf>>>(),
        "Option<Limit>" => describe_default::<Option<Limit>>(),
        "Box<Leaf>" => describe_default::<Box<Leaf>>(),
        "Arc<Leaf>" => describe_default::<Arc<Leaf>>(),
        "(Mode, Box<Limit>)" => describe_default::<(Mode, Box<Limit>)>(),
        "[Mode; 3]" => describe_default::<[Mode; 3]>(),
        "[Limit; 2]" => describe_default::<[Limit; 2]>(),
        "Cell<Mode>" => describe_default::<Cell<Mode>>(),
        "RefCell<Vec<Mode>>" => describe_default::<RefCell<Vec<Mode>>>(),
        "Reverse<Mode>" => describe_default::<Reverse<Mode>>(),
        "Vec<Mode>" => describe_default::<Vec<Mode>>(),
        "HashMap<String, Mode>" => describe_default::<HashMap<String, Mode>>(),
        "BTreeMap<String, Limit>" => describe_default::<BTreeMap<String, Limit>>(),
        "char" => describe_default::<char>(),
        "f64" => describe_default::<f64>(),
        "u64" => describe_default::<u64>(),
        "bool" => describe_default::<bool>(),
        "String" => describe_default::<String>(),
        "()" => describe_default::<()>(),
        _ => return None,
    })
}

// Tracked values: Arc scripts and Holder::clone.

/// What happened to the `Tracked` values of one request.
#[derive(Default)]
struct TrackLog {
    next_id: u32,
    dropped: Vec<u32>,
    cloned: Vec<(u32, u32)>,
}

thread_local! {
    static TRACK_LOG: RefCell<TrackLog> = RefCell::new(TrackLog::default());
}

fn reset_track_log() {
    TRACK_LOG.with(|log| *log.borrow_mut() = TrackLog::default());
}

fn next_id() -> u32 {
    TRACK_LOG.with(|log| {
        let mut log = log.borrow_mut();
        log.next_id += 1;
        log.next_id
    })
}

fn take_dropped() -> Vec<u32> {
    TRACK_LOG.with(|log| std::mem::take(&mut log.borrow_mut().dropped))
}

fn take_cloned() -> Vec<(u32, u32)> {
    TRACK_LOG.with(|log| std::mem::take(&mut log.borrow_mut().cloned))
}

/// A value that says when it is cloned and dropped. Each value, the clones
/// included, gets the next id; equality is by `value` alone.
struct Tracked {
    id: u32,
    value: i64,
}

impl Tracked {
    fn new(value: i64) -> Self {
        Tracked { id: next_id(), value }
    }

    fn json(&self) -> Value {
        json!([self.id, self.value])
    }
}

impl Clone for Tracked {
    fn clone(&self) -> Self {
        let id = next_id();
        TRACK_LOG.with(|log| log.borrow_mut().cloned.push((self.id, id)));
        Tracked { id, value: self.value }
    }
}

impl Drop for Tracked {
    fn drop(&mut self) {
        let id = self.id;
        let _ = TRACK_LOG.try_with(|log| log.borrow_mut().dropped.push(id));
    }
}

impl PartialEq for Tracked {
    fn eq(&self, other: &Self) -> bool {
        self.value == other.value
    }
}

/// A struct holding Tracked values every way a derived Clone treats
/// differently: boxes are copied, Arcs shared.
#[derive(Clone)]
struct Holder {
    boxed: Box<Tracked>,
    shared: Arc<Tracked>,
    maybe: Option<Arc<Tracked>>,
    maybe_boxed: Option<Box<Tracked>>,
    list: Vec<Arc<Tracked>>,
    owned: Vec<Tracked>,
    bytes: Arc<[u8]>,
    counted: Arc<Tracked>,
    weak: Weak<Tracked>,
    cell: Cell<u32>,
    refcell: RefCell<Vec<u32>>,
    nested: Vec<Option<Arc<Tracked>>>,
    pair: (Arc<Tracked>, String),
    boxes: (Box<Tracked>, u8),
}

fn clone_holder(n: usize) -> Value {
    reset_track_log();
    // Built one value at a time, so that the ids are given in this order.
    let boxed = Box::new(Tracked::new(1));
    let shared = Arc::new(Tracked::new(2));
    let maybe = Some(Arc::new(Tracked::new(3)));
    let maybe_boxed = Some(Box::new(Tracked::new(4)));
    let list: Vec<Arc<Tracked>> = (0..n).map(|i| Arc::new(Tracked::new(10 + i as i64))).collect();
    let owned: Vec<Tracked> = (0..n).map(|i| Tracked::new(20 + i as i64)).collect();
    let bytes: Arc<[u8]> = Arc::from(&b"shared bytes"[..]);
    let counted = Arc::new(Tracked::new(5));
    let weak = Arc::downgrade(&counted);
    let nested: Vec<Option<Arc<Tracked>>> =
        (0..n).map(|i| (i % 2 == 0).then(|| Arc::new(Tracked::new(30 + i as i64)))).collect();
    let pair = (Arc::new(Tracked::new(6)), "pair".to_owned());
    let boxes = (Box::new(Tracked::new(7)), 8u8);
    let holder = Holder {
        boxed,
        shared,
        maybe,
        maybe_boxed,
        list,
        owned,
        bytes,
        counted,
        weak,
        cell: Cell::new(9),
        refcell: RefCell::new(vec![1, 2]),
        nested,
        pair,
        boxes,
    };

    let clone = holder.clone();
    let cloned = take_cloned();
    let both_some = |a: &Option<Arc<Tracked>>, b: &Option<Arc<Tracked>>| match (a, b) {
        (Some(a), Some(b)) => json!(Arc::ptr_eq(a, b)),
        _ => Value::Null,
    };
    let shared = json!({
        "boxed": std::ptr::eq(&*holder.boxed, &*clone.boxed),
        "shared": Arc::ptr_eq(&holder.shared, &clone.shared),
        "maybe": both_some(&holder.maybe, &clone.maybe),
        "maybe_boxed": std::ptr::eq(&**holder.maybe_boxed.as_ref().unwrap(), &**clone.maybe_boxed.as_ref().unwrap()),
        "list": holder.list.iter().zip(&clone.list).map(|(a, b)| Arc::ptr_eq(a, b)).collect::<Vec<_>>(),
        "list_buffer": n > 0 && std::ptr::eq(holder.list.as_ptr(), clone.list.as_ptr()),
        "owned": holder.owned.iter().zip(&clone.owned).map(|(a, b)| std::ptr::eq(a, b)).collect::<Vec<_>>(),
        "bytes": Arc::ptr_eq(&holder.bytes, &clone.bytes),
        "counted": Arc::ptr_eq(&holder.counted, &clone.counted),
        "weak": holder.weak.ptr_eq(&clone.weak),
        "nested": holder.nested.iter().zip(&clone.nested).map(|(a, b)| both_some(a, b)).collect::<Vec<_>>(),
        "pair": Arc::ptr_eq(&holder.pair.0, &clone.pair.0),
        "boxes": std::ptr::eq(&*holder.boxes.0, &*clone.boxes.0),
    });
    let copied = json!({
        "boxed": clone.boxed.json(),
        "maybe_boxed": clone.maybe_boxed.as_ref().map(|t| t.json()),
        "owned": clone.owned.iter().map(Tracked::json).collect::<Vec<_>>(),
        "cell": clone.cell.get(),
        "refcell": *clone.refcell.borrow(),
        "pair": clone.pair.1,
        "boxes": [clone.boxes.0.json(), clone.boxes.1],
    });
    let counts = json!([Arc::strong_count(&holder.counted), Arc::weak_count(&holder.counted)]);
    drop(clone);
    let dropped = take_dropped();
    let after = json!([Arc::strong_count(&holder.counted), Arc::weak_count(&holder.counted)]);
    json!({
        "cloned": cloned,
        "shared": shared,
        "copied": copied,
        "counts": counts,
        "dropped": dropped,
        "after": after,
    })
}

/// Runs a script of steps on Arc and Weak handles. Each step that makes a
/// handle appends a slot to `arcs` or `weaks`, live or not, so that the
/// indices of later steps do not depend on the results of earlier ones. A
/// step on a slot that holds no handle observes nothing (null).
///
/// - `["new", V]`: `Arc::new(Tracked::new(V))`; the id.
/// - `["clone", I]`: `arcs[I].clone()`.
/// - `["drop", I]`: drops `arcs[I]`.
/// - `["downgrade", I]`: `Arc::downgrade(&arcs[I])`.
/// - `["weak_new"]`: `Weak::new()`.
/// - `["upgrade", J]`: `weaks[J].upgrade()`; whether it gave an Arc.
/// - `["weak_clone", J]`, `["weak_drop", J]`.
/// - `["counts", I]`: `[Arc::strong_count, Arc::weak_count]`.
/// - `["weak_counts", J]`: `[weak.strong_count(), weak.weak_count()]`.
/// - `["get_mut", I, V]`: sets the value through `Arc::get_mut`; whether it
///   could.
/// - `["make_mut", I, V]`: sets the value through `Arc::make_mut`; the id of
///   the value changed.
/// - `["try_unwrap", I]`, `["into_inner", I]`, `["unwrap_or_clone", I]`: the
///   value taken out (and then dropped), or false.
/// - `["ptr_eq", I, K]`, `["weak_ptr_eq", J, L]`, `["eq", I, K]`.
/// - `["get", I]`: the value.
/// - `["forget", I]`, `["weak_forget", J]`: `mem::forget`.
fn arc_script(steps: &[Value]) -> Value {
    reset_track_log();
    let mut arcs: Vec<Option<Arc<Tracked>>> = Vec::new();
    let mut weaks: Vec<Option<Weak<Tracked>>> = Vec::new();
    let mut observed = Vec::new();
    for step in steps {
        let s = parts(step);
        let index = |k: usize| s[k].as_u64().expect("an index") as usize;
        let number = |k: usize| s[k].as_i64().expect("a value");
        let result = match s[0].as_str().expect("a step") {
            "new" => {
                let arc = Arc::new(Tracked::new(number(1)));
                let id = arc.id;
                arcs.push(Some(arc));
                json!(id)
            }
            "clone" => {
                let clone = arcs[index(1)].clone();
                let live = clone.is_some();
                arcs.push(clone);
                if live { json!(true) } else { Value::Null }
            }
            "drop" => {
                arcs[index(1)] = None;
                Value::Null
            }
            "downgrade" => {
                let weak = arcs[index(1)].as_ref().map(Arc::downgrade);
                let live = weak.is_some();
                weaks.push(weak);
                if live { json!(true) } else { Value::Null }
            }
            "weak_new" => {
                weaks.push(Some(Weak::new()));
                Value::Null
            }
            "upgrade" => match &weaks[index(1)] {
                Some(weak) => {
                    let upgraded = weak.upgrade();
                    let some = upgraded.is_some();
                    arcs.push(upgraded);
                    json!(some)
                }
                None => {
                    arcs.push(None);
                    Value::Null
                }
            },
            "weak_clone" => {
                let clone = weaks[index(1)].clone();
                let live = clone.is_some();
                weaks.push(clone);
                if live { json!(true) } else { Value::Null }
            }
            "weak_drop" => {
                weaks[index(1)] = None;
                Value::Null
            }
            "counts" => match &arcs[index(1)] {
                Some(arc) => json!([Arc::strong_count(arc), Arc::weak_count(arc)]),
                None => Value::Null,
            },
            "weak_counts" => match &weaks[index(1)] {
                Some(weak) => json!([weak.strong_count(), weak.weak_count()]),
                None => Value::Null,
            },
            "get_mut" => match arcs[index(1)].as_mut() {
                Some(arc) => match Arc::get_mut(arc) {
                    Some(tracked) => {
                        tracked.value = number(2);
                        json!(true)
                    }
                    None => json!(false),
                },
                None => Value::Null,
            },
            "make_mut" => match arcs[index(1)].as_mut() {
                Some(arc) => {
                    let tracked = Arc::make_mut(arc);
                    tracked.value = number(2);
                    json!(tracked.id)
                }
                None => Value::Null,
            },
            "try_unwrap" => {
                let i = index(1);
                match arcs[i].take() {
                    Some(arc) => match Arc::try_unwrap(arc) {
                        Ok(tracked) => tracked.json(),
                        Err(arc) => {
                            arcs[i] = Some(arc);
                            json!(false)
                        }
                    },
                    None => Value::Null,
                }
            }
            "into_inner" => match arcs[index(1)].take() {
                Some(arc) => match Arc::into_inner(arc) {
                    Some(tracked) => tracked.json(),
                    None => json!(false),
                },
                None => Value::Null,
            },
            "unwrap_or_clone" => match arcs[index(1)].take() {
                Some(arc) => Arc::unwrap_or_clone(arc).json(),
                None => Value::Null,
            },
            "ptr_eq" => match (&arcs[index(1)], &arcs[index(2)]) {
                (Some(a), Some(b)) => json!(Arc::ptr_eq(a, b)),
                _ => Value::Null,
            },
            "weak_ptr_eq" => match (&weaks[index(1)], &weaks[index(2)]) {
                (Some(a), Some(b)) => json!(Weak::ptr_eq(a, b)),
                _ => Value::Null,
            },
            "eq" => match (&arcs[index(1)], &arcs[index(2)]) {
                (Some(a), Some(b)) => json!(a == b),
                _ => Value::Null,
            },
            "get" => match &arcs[index(1)] {
                Some(arc) => arc.json(),
                None => Value::Null,
            },
            "forget" => {
                if let Some(arc) = arcs[index(1)].take() {
                    std::mem::forget(arc);
                }
                Value::Null
            }
            "weak_forget" => {
                if let Some(weak) = weaks[index(1)].take() {
                    std::mem::forget(weak);
                }
                Value::Null
            }
            other => panic!("not an Arc step: {other}"),
        };
        observed.push(json!([result, take_dropped()]));
    }
    // The handles left go in order, the Arcs first.
    for slot in arcs.drain(..) {
        drop(slot);
    }
    for slot in weaks.drain(..) {
        drop(slot);
    }
    json!({ "steps": observed, "final": take_dropped() })
}
