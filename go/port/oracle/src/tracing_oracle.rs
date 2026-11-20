//! Oracle module `tracing`. Requests are objects with an `"op"` member; see the
//! Go tests that use this module for the operations it serves
//! (`go/crates/tracing` and `go/crates/tracingsubscriber`).
//!
//! `run` builds a `tracing_subscriber::fmt()` subscriber from a description,
//! runs a script of spans and events under it, and replies with the bytes the
//! subscriber wrote and with which spans and `enabled!` probes were enabled.
//! `filter` and `directive` parse `EnvFilter` text; `level` parses a level;
//! `now` writes one line with the default timer.

use std::collections::HashMap;
use std::error::Error as StdError;
use std::fmt::{self, Debug, Display};
use std::io;
use std::str::FromStr;
use std::sync::atomic::{AtomicU8, Ordering};
use std::sync::{Arc, Mutex, OnceLock};

use serde_json::{Value, json};
use tracing::Span;
use tracing::level_filters::LevelFilter;
use tracing::span::EnteredSpan;
use tracing::callsite::{self, Callsite, Identifier};
use tracing::field::{self, FieldSet};
use tracing::metadata::Kind;
use tracing::subscriber::Interest;
use tracing::{Dispatch, Event, Level, Metadata, Subscriber, dispatcher};
use tracing_subscriber::EnvFilter;
use tracing_subscriber::filter::Directive;
use tracing_subscriber::fmt::format::{FmtSpan, Writer};
use tracing_subscriber::fmt::time::FormatTime;
use tracing_subscriber::fmt::MakeWriter;

fn leak(text: &str) -> &'static str {
    Box::leak(text.to_owned().into_boxed_str())
}

// === callsites made at run time ===========================================

/// What the `tracing` macros declare as a static: here made from a request.
struct DynCallsite {
    interest: AtomicU8,
    meta: OnceLock<&'static Metadata<'static>>,
}

impl Callsite for DynCallsite {
    fn set_interest(&self, interest: Interest) {
        let value = if interest.is_never() {
            0
        } else if interest.is_always() {
            2
        } else {
            1
        };
        self.interest.store(value, Ordering::SeqCst);
    }

    fn metadata(&self) -> &Metadata<'_> {
        self.meta.get().expect("metadata is set before the callsite is used")
    }
}

impl DynCallsite {
    fn new(name: &str, target: &str, level: Level, names: &[String], kind: Kind) -> &'static Self {
        let callsite: &'static DynCallsite =
            Box::leak(Box::new(DynCallsite { interest: AtomicU8::new(0xFF), meta: OnceLock::new() }));
        let names: &'static [&'static str] =
            Box::leak(names.iter().map(|name| leak(name)).collect::<Vec<_>>().into_boxed_slice());
        let meta: &'static Metadata<'static> = Box::leak(Box::new(Metadata::new(
            leak(name),
            leak(target),
            level,
            None,
            None,
            None,
            FieldSet::new(names, Identifier(callsite)),
            kind,
        )));
        let _ = callsite.meta.set(meta);
        callsite::register(callsite);
        callsite
    }

    fn meta(&self) -> &'static Metadata<'static> {
        self.meta.get().expect("metadata is set")
    }

    /// The check every macro makes before it builds an event or a span.
    fn enabled(&self) -> bool {
        let meta = self.meta();
        *meta.level() <= LevelFilter::current() && {
            let interest = self.interest.load(Ordering::SeqCst);
            interest != 0 && (interest == 2 || dispatcher::get_default(|current| current.enabled(meta)))
        }
    }
}

// === values ================================================================

/// A value with a Debug form, for `?value` fields.
enum Dbg {
    Str(String),
    Int(i64),
    Float(f64),
    Bool(bool),
    ListStr(Vec<String>),
    ListInt(Vec<i64>),
    OptStr(Option<String>),
    OptInt(Option<i64>),
    Unit,
}

impl Debug for Dbg {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Dbg::Str(v) => Debug::fmt(v, f),
            Dbg::Int(v) => Debug::fmt(v, f),
            Dbg::Float(v) => Debug::fmt(v, f),
            Dbg::Bool(v) => Debug::fmt(v, f),
            Dbg::ListStr(v) => Debug::fmt(v, f),
            Dbg::ListInt(v) => Debug::fmt(v, f),
            Dbg::OptStr(v) => Debug::fmt(v, f),
            Dbg::OptInt(v) => Debug::fmt(v, f),
            Dbg::Unit => Debug::fmt(&(), f),
        }
    }
}

/// An error with a source chain, for values recorded as `dyn Error`.
#[derive(Debug)]
struct Typed {
    text: String,
    source: Option<Box<Typed>>,
}

impl Display for Typed {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.text)
    }
}

impl StdError for Typed {
    fn source(&self) -> Option<&(dyn StdError + 'static)> {
        self.source.as_deref().map(|source| source as &(dyn StdError + 'static))
    }
}

fn typed(texts: &[String]) -> Typed {
    let mut error: Option<Box<Typed>> = None;
    for text in texts.iter().rev() {
        error = Some(Box::new(Typed { text: text.clone(), source: error }));
    }
    *error.expect("an error value needs at least one text")
}

/// A field value owned by the oracle, lent to tracing as `&dyn Value`.
enum Held {
    I64(i64),
    U64(u64),
    F64(f64),
    Bool(bool),
    Str(String),
    Display(field::DisplayValue<String>),
    Debug(field::DebugValue<Dbg>),
    Error(Typed),
    Bytes(Vec<u8>),
    Empty,
}

impl Held {
    fn parse(spec: &Value) -> Option<Held> {
        let value = spec.get("value").unwrap_or(&Value::Null);
        let text = || value.as_str().unwrap_or_default().to_owned();
        let texts = || -> Vec<String> {
            value
                .as_array()
                .map(|items| items.iter().map(|item| item.as_str().unwrap_or_default().to_owned()).collect())
                .unwrap_or_default()
        };
        let ints = || -> Vec<i64> {
            value.as_array().map(|items| items.iter().filter_map(Value::as_i64).collect()).unwrap_or_default()
        };
        Some(match crate::text(spec, "kind") {
            "i64" => Held::I64(value.as_i64()?),
            "u64" => Held::U64(value.as_u64()?),
            "f64" => Held::F64(float(value)?),
            "bool" => Held::Bool(value.as_bool()?),
            "str" => Held::Str(text()),
            "display" => Held::Display(field::display(text())),
            "debug_str" => Held::Debug(field::debug(Dbg::Str(text()))),
            "debug_int" => Held::Debug(field::debug(Dbg::Int(value.as_i64()?))),
            "debug_float" => Held::Debug(field::debug(Dbg::Float(float(value)?))),
            "debug_bool" => Held::Debug(field::debug(Dbg::Bool(value.as_bool()?))),
            "debug_list_str" => Held::Debug(field::debug(Dbg::ListStr(texts()))),
            "debug_list_int" => Held::Debug(field::debug(Dbg::ListInt(ints()))),
            "debug_opt_str" => {
                Held::Debug(field::debug(Dbg::OptStr(value.as_str().map(ToOwned::to_owned))))
            }
            "debug_opt_int" => Held::Debug(field::debug(Dbg::OptInt(value.as_i64()))),
            "debug_unit" => Held::Debug(field::debug(Dbg::Unit)),
            "error" => Held::Error(typed(&texts())),
            "bytes" => Held::Bytes(value.as_array()?.iter().filter_map(Value::as_u64).map(|b| b as u8).collect()),
            "empty" => Held::Empty,
            _ => return None,
        })
    }
}

/// A float from JSON, where the three values JSON cannot hold are strings.
fn float(value: &Value) -> Option<f64> {
    match value {
        Value::String(text) => match text.as_str() {
            "NaN" => Some(f64::NAN),
            "inf" => Some(f64::INFINITY),
            "-inf" => Some(f64::NEG_INFINITY),
            "-0" => Some(-0.0),
            _ => None,
        },
        other => other.as_f64(),
    }
}

fn parse_fields(spec: &Value) -> Option<(Vec<String>, Vec<Held>)> {
    let mut names = Vec::new();
    let mut held = Vec::new();
    for field in spec.get("fields").and_then(Value::as_array).into_iter().flatten() {
        names.push(crate::text(field, "name").to_owned());
        held.push(Held::parse(field)?);
    }
    Some((names, held))
}

fn with_values<T>(held: &[Held], run: impl FnOnce(&[Option<&dyn field::Value>]) -> T) -> T {
    let slices: Vec<&[u8]> = held
        .iter()
        .map(|value| match value {
            Held::Bytes(bytes) => bytes.as_slice(),
            _ => &[][..],
        })
        .collect();
    let errors: Vec<Option<&(dyn StdError + 'static)>> = held
        .iter()
        .map(|value| match value {
            Held::Error(error) => Some(error as &(dyn StdError + 'static)),
            _ => None,
        })
        .collect();
    let values: Vec<Option<&dyn field::Value>> = held
        .iter()
        .enumerate()
        .map(|(index, value)| -> Option<&dyn field::Value> {
            match value {
                Held::I64(v) => Some(v),
                Held::U64(v) => Some(v),
                Held::F64(v) => Some(v),
                Held::Bool(v) => Some(v),
                Held::Str(v) => Some(v),
                Held::Display(v) => Some(v),
                Held::Debug(v) => Some(v),
                Held::Error(_) => errors[index].as_ref().map(|error| error as &dyn field::Value),
                Held::Bytes(_) => Some(&slices[index]),
                Held::Empty => None,
            }
        })
        .collect();
    run(&values)
}

// === messages ==============================================================

/// A message is a format string known at compile time. The request names one
/// of these and gives its arguments; the Go side passes the same format
/// string to the event function.
fn message(spec: &Value) -> Option<String> {
    let args: Vec<&Value> = spec.get("args").and_then(Value::as_array).map(|a| a.iter().collect()).unwrap_or_default();
    let s = |index: usize| args.get(index).and_then(|v| v.as_str()).unwrap_or_default().to_owned();
    let i = |index: usize| args.get(index).and_then(|v| v.as_i64()).unwrap_or_default();
    let f = |index: usize| args.get(index).and_then(|v| float(v)).unwrap_or_default();
    Some(match crate::text(spec, "format") {
        // A literal: only its escaped braces are read.
        "" => crate::text(spec, "literal").replace("{{", "{").replace("}}", "}"),
        "{}" => format!("{}", s(0)),
        "connected to {} in {}ms" => format!("connected to {} in {}ms", s(0), i(1)),
        "value={:?} other={:?}" => format!("value={:?} other={:?}", s(0), i(1)),
        "[{:>8}] [{:<6}] [{:^7}]" => format!("[{:>8}] [{:<6}] [{:^7}]", s(0), s(1), s(2)),
        "{:05} {:#x} {:.3}" => format!("{:05} {:#x} {:.3}", i(0), i(1), f(2)),
        "{0} {0} {1}" => format!("{0} {0} {1}", s(0), s(1)),
        "multi\nline {}" => format!("multi\nline {}", s(0)),
        "{name} is {age}" => {
            let (name, age) = (s(0), i(1));
            format!("{name} is {age}")
        }
        "{{braces}} {} {{}}" => format!("{{braces}} {} {{}}", s(0)),
        _ => return None,
    })
}

// === the subscriber ========================================================

#[derive(Clone, Default)]
struct Buffer(Arc<Mutex<Vec<u8>>>);

impl io::Write for Buffer {
    fn write(&mut self, bytes: &[u8]) -> io::Result<usize> {
        self.0.lock().unwrap().extend_from_slice(bytes);
        Ok(bytes.len())
    }

    fn flush(&mut self) -> io::Result<()> {
        Ok(())
    }
}

impl<'a> MakeWriter<'a> for Buffer {
    type Writer = Buffer;

    fn make_writer(&'a self) -> Self::Writer {
        self.clone()
    }
}

struct FixedTime;

impl FormatTime for FixedTime {
    fn format_time(&self, w: &mut Writer<'_>) -> fmt::Result {
        w.write_str("2026-11-20T11:36:12.123456Z")
    }
}

fn level(text: &str) -> Option<Level> {
    Level::from_str(text).ok()
}

fn span_events(text: &str) -> FmtSpan {
    let mut kind = FmtSpan::NONE;
    for part in text.split('|') {
        kind |= match part {
            "new" => FmtSpan::NEW,
            "enter" => FmtSpan::ENTER,
            "exit" => FmtSpan::EXIT,
            "close" => FmtSpan::CLOSE,
            _ => FmtSpan::NONE,
        };
    }
    kind
}

fn env_filter(spec: &Value) -> Result<EnvFilter, String> {
    let directives = crate::text(spec, "directives");
    let builder = EnvFilter::builder().with_regex(spec.get("regex").and_then(Value::as_bool).unwrap_or(true));
    match crate::text(spec, "mode") {
        // EnvFilter::new
        "new" => Ok(builder.with_default_directive(LevelFilter::ERROR.into()).parse_lossy(directives)),
        // EnvFilter::try_new
        _ => builder.parse(directives).map_err(|error| error.to_string()),
    }
}

fn hint_text(hint: Option<LevelFilter>) -> Value {
    hint.map_or(Value::Null, |hint| Value::String(hint.to_string()))
}

/// Builds the dispatcher a `config` describes, and returns it with its max
/// level hint. An `Err` is the text of the filter's parse error.
fn subscriber(config: &Value, buffer: &Buffer) -> Result<(Dispatch, Option<LevelFilter>), String> {
    let flag = |name: &str, default: bool| config.get(name).and_then(Value::as_bool).unwrap_or(default);
    let builder = tracing_subscriber::fmt()
        .with_writer(buffer.clone())
        .with_ansi(flag("ansi", false))
        .with_level(flag("level", true))
        .with_target(flag("target", true))
        .with_ansi_sanitization(flag("sanitize", true))
        .with_span_events(span_events(crate::text(config, "span_events")));

    macro_rules! finish {
        ($builder:expr) => {{
            let builder = $builder;
            match config.get("filter") {
                Some(filter) if filter.get("directives").is_some() => {
                    let subscriber = builder.with_env_filter(env_filter(filter)?).finish();
                    let hint = Subscriber::max_level_hint(&subscriber);
                    (Dispatch::new(subscriber), hint)
                }
                Some(filter) if filter.get("max_level").is_some() => {
                    let max = LevelFilter::from_str(crate::text(filter, "max_level")).map_err(|e| e.to_string())?;
                    let subscriber = builder.with_max_level(max).finish();
                    let hint = Subscriber::max_level_hint(&subscriber);
                    (Dispatch::new(subscriber), hint)
                }
                _ => {
                    let subscriber = builder.finish();
                    let hint = Subscriber::max_level_hint(&subscriber);
                    (Dispatch::new(subscriber), hint)
                }
            }
        }};
    }

    Ok(match crate::text(config, "time") {
        "none" => finish!(builder.without_time()),
        "system" => finish!(builder),
        _ => finish!(builder.with_timer(FixedTime)),
    })
}

// === the script ============================================================

fn run(request: &Value) -> Value {
    let buffer = Buffer::default();
    let empty = json!({});
    let config = request.get("config").unwrap_or(&empty);
    let (dispatch, hint) = match subscriber(config, &buffer) {
        Ok(built) => built,
        Err(error) => return json!({ "filter_error": error }),
    };

    let mut spans: HashMap<String, Span> = HashMap::new();
    let mut span_enabled = serde_json::Map::new();
    let mut entered: Vec<(String, EnteredSpan)> = Vec::new();
    let mut probes = Vec::new();
    let mut failed = None;

    dispatcher::with_default(&dispatch, || {
        for action in request.get("actions").and_then(Value::as_array).into_iter().flatten() {
            let id = crate::text(action, "id").to_owned();
            let step_level = level(crate::text(action, "level")).unwrap_or(Level::INFO);
            let target = crate::text(action, "target");
            match crate::text(action, "do") {
                "event" => {
                    let Some((mut names, mut held)) = parse_fields(action) else {
                        failed = Some(action.clone());
                        return;
                    };
                    if let Some(spec) = action.get("message").filter(|spec| !spec.is_null()) {
                        let Some(text) = message(spec) else {
                            failed = Some(action.clone());
                            return;
                        };
                        // The macros put the message first.
                        names.insert(0, "message".to_owned());
                        held.insert(0, Held::Display(field::display(text)));
                    }
                    let callsite = DynCallsite::new("event", target, step_level, &names, Kind::EVENT);
                    if callsite.enabled() {
                        let meta = callsite.meta();
                        with_values(&held, |values| {
                            let values = meta.fields().value_set_all(values);
                            match crate::text(action, "parent") {
                                "" | "current" => Event::dispatch(meta, &values),
                                "root" => Event::child_of(None::<tracing::span::Id>, meta, &values),
                                parent => Event::child_of(spans.get(parent).and_then(Span::id), meta, &values),
                            }
                        });
                    }
                }
                "span" => {
                    let Some((names, held)) = parse_fields(action) else {
                        failed = Some(action.clone());
                        return;
                    };
                    let callsite =
                        DynCallsite::new(crate::text(action, "name"), target, step_level, &names, Kind::SPAN);
                    let span = if callsite.enabled() {
                        let meta = callsite.meta();
                        with_values(&held, |values| {
                            let values = meta.fields().value_set_all(values);
                            match crate::text(action, "parent") {
                                "" | "current" => Span::new(meta, &values),
                                "root" => Span::new_root(meta, &values),
                                parent => Span::child_of(spans.get(parent).and_then(Span::id), meta, &values),
                            }
                        })
                    } else {
                        Span::none()
                    };
                    span_enabled.insert(id.clone(), Value::Bool(!span.is_disabled()));
                    spans.insert(id, span);
                }
                "enter" => {
                    if let Some(span) = spans.get(&id) {
                        entered.push((id, span.clone().entered()));
                    }
                }
                "exit" => {
                    if let Some(position) = entered.iter().rposition(|(entered_id, _)| *entered_id == id) {
                        drop(entered.remove(position));
                    }
                }
                "record" => {
                    let Some(held) = Held::parse(action) else {
                        failed = Some(action.clone());
                        return;
                    };
                    if let Some(span) = spans.get(&id) {
                        with_values(std::slice::from_ref(&held), |values| {
                            if let Some(value) = values[0] {
                                span.record(crate::text(action, "name"), value);
                            }
                        });
                    }
                }
                "close" => {
                    spans.remove(&id);
                }
                "enabled" => {
                    // enabled!(target: .., level)
                    let callsite = DynCallsite::new("enabled", target, step_level, &[], Kind::EVENT.hint());
                    let enabled =
                        callsite.enabled() && dispatcher::get_default(|current| current.enabled(callsite.meta()));
                    probes.push(Value::Bool(enabled));
                }
                _ => {
                    failed = Some(action.clone());
                    return;
                }
            }
        }
        // Rust drops what the script left open, innermost first.
        while let Some(guard) = entered.pop() {
            drop(guard);
        }
        spans.clear();
    });

    if let Some(action) = failed {
        return crate::unknown(&action);
    }
    drop(dispatch);
    let output = String::from_utf8_lossy(&buffer.0.lock().unwrap()).into_owned();
    json!({
        "output": output,
        "hint": hint_text(hint),
        "spans": span_enabled,
        "enabled": probes,
    })
}

// === parsing ===============================================================

fn filter(request: &Value) -> Value {
    match env_filter(request) {
        Ok(filter) => json!({
            "display": filter.to_string(),
            "hint": hint_text(filter.max_level_hint()),
        }),
        Err(error) => json!({ "error": error }),
    }
}

fn directive(request: &Value) -> Value {
    match Directive::from_str(crate::text(request, "text")) {
        Ok(directive) => json!({ "display": directive.to_string() }),
        Err(error) => {
            json!({ "error": error.to_string(), "source": error.source().map(ToString::to_string) })
        }
    }
}

fn parse_level(request: &Value) -> Value {
    let text = crate::text(request, "text");
    let level = match Level::from_str(text) {
        Ok(level) => json!({ "display": level.to_string(), "debug": format!("{level:?}"), "padded": format!("[{level:>7}]") }),
        Err(error) => json!({ "error": error.to_string() }),
    };
    let filter = match LevelFilter::from_str(text) {
        Ok(filter) => json!({ "display": filter.to_string(), "debug": format!("{filter:?}"), "padded": format!("[{filter:<7}]") }),
        Err(error) => json!({ "error": error.to_string() }),
    };
    json!({ "level": level, "filter": filter })
}

/// One line written with the default timer, between two readings of the
/// clock.
fn now() -> Value {
    let stamp = || {
        let since = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).unwrap();
        json!({ "secs": since.as_secs(), "nanos": since.subsec_nanos() })
    };
    let buffer = Buffer::default();
    let subscriber = tracing_subscriber::fmt().with_writer(buffer.clone()).with_ansi(false).finish();
    let dispatch = Dispatch::new(subscriber);
    let before = stamp();
    dispatcher::with_default(&dispatch, || {
        let callsite = DynCallsite::new("event", "clock", Level::INFO, &["message".to_owned()], Kind::EVENT);
        if callsite.enabled() {
            let meta = callsite.meta();
            let text = field::display("tick");
            let values = [Some(&text as &dyn field::Value)];
            Event::dispatch(meta, &meta.fields().value_set_all(&values));
        }
    });
    let after = stamp();
    let line = String::from_utf8_lossy(&buffer.0.lock().unwrap()).into_owned();
    json!({ "line": line, "before": before, "after": after })
}

pub fn handle(request: &Value) -> Value {
    match crate::text(request, "op") {
        "run" => run(request),
        "filter" => filter(request),
        "directive" => directive(request),
        "level" => parse_level(request),
        "now" => now(),
        _ => crate::unknown(request),
    }
}
