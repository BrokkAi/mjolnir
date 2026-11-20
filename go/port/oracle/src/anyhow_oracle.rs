//! Oracle module `anyhow`. Requests are objects with an `"op"` member; see the
//! Go tests that use this module for the operations it serves
//! (`go/crates/anyhow/oracle_test.go`).
//!
//! `{"op": "error", "root": {..}, "layers": [{..}, ..]}` builds an
//! `anyhow::Error` from a root and context layers (innermost first) and
//! replies with the forms built from plain messages, its chain, and what it
//! downcasts to. `"error_fmt"` builds the same error and replies with the
//! forms that go through `Formatter`: `{:#?}`, widths, and the Debug form of
//! each chain element.
//!
//! `{"op": "ensure", ..}` replies with the message `ensure!` builds for a
//! failed comparison.

use std::error::Error as StdError;
use std::fmt::{self, Debug, Display};
use std::io;

use anyhow::{Context, Error};
use serde_json::{Map, Value, json};

/// A `std::error::Error` with a source chain: the stand-in for an error type
/// product code defines.
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

/// A context value that is not an error: `Display` only matters to anyhow.
#[derive(Debug)]
struct TypedCtx {
    text: String,
}

impl Display for TypedCtx {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.text)
    }
}

/// A message value for `Error::msg` that is neither a string nor an error.
#[derive(Debug)]
struct TypedMsg {
    text: String,
}

impl Display for TypedMsg {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.text)
    }
}

fn leak(text: &str) -> &'static str {
    Box::leak(text.to_owned().into_boxed_str())
}

fn texts(spec: &Value) -> Vec<String> {
    spec.get("texts")
        .and_then(Value::as_array)
        .map(|items| items.iter().map(|item| item.as_str().unwrap_or_default().to_owned()).collect())
        .unwrap_or_default()
}

fn typed(texts: &[String]) -> Typed {
    let mut error: Option<Box<Typed>> = None;
    for text in texts.iter().rev() {
        error = Some(Box::new(Typed { text: text.clone(), source: error }));
    }
    *error.expect("a typed error needs at least one text")
}

fn io_kind(name: &str) -> io::ErrorKind {
    match name {
        "NotFound" => io::ErrorKind::NotFound,
        "PermissionDenied" => io::ErrorKind::PermissionDenied,
        "ConnectionRefused" => io::ErrorKind::ConnectionRefused,
        "AlreadyExists" => io::ErrorKind::AlreadyExists,
        "InvalidInput" => io::ErrorKind::InvalidInput,
        "InvalidData" => io::ErrorKind::InvalidData,
        "TimedOut" => io::ErrorKind::TimedOut,
        "UnexpectedEof" => io::ErrorKind::UnexpectedEof,
        "BrokenPipe" => io::ErrorKind::BrokenPipe,
        _ => io::ErrorKind::Other,
    }
}

/// A root that is a `std::error::Error`, before it becomes an `anyhow::Error`.
enum StdRoot {
    Io(io::Error),
    Typed(Typed),
}

enum Root {
    Std(StdRoot),
    Anyhow(Error),
}

fn root(spec: &Value) -> Option<Root> {
    let text = crate::text(spec, "text");
    Some(match crate::text(spec, "kind") {
        "literal" => Root::Anyhow(Error::msg(leak(text))),
        "string" => Root::Anyhow(Error::msg(text.to_owned())),
        "msg_typed" => Root::Anyhow(Error::msg(TypedMsg { text: text.to_owned() })),
        "option" => Root::Anyhow(None::<()>.context(text.to_owned()).unwrap_err()),
        "option_typed" => {
            Root::Anyhow(None::<()>.with_context(|| TypedCtx { text: text.to_owned() }).unwrap_err())
        }
        "io_os" => Root::Std(StdRoot::Io(io::Error::from_raw_os_error(
            spec.get("code").and_then(Value::as_i64).unwrap_or_default() as i32,
        ))),
        "io_custom" => {
            Root::Std(StdRoot::Io(io::Error::new(io_kind(crate::text(spec, "io_kind")), text.to_owned())))
        }
        "io_simple" => Root::Std(StdRoot::Io(io::Error::from(io_kind(crate::text(spec, "io_kind"))))),
        "typed" => Root::Std(StdRoot::Typed(typed(&texts(spec)))),
        "boxed" => {
            let boxed: Box<dyn StdError + Send + Sync> = Box::new(typed(&texts(spec)));
            Root::Anyhow(Error::from_boxed(boxed))
        }
        "boxed_str" => {
            let boxed = Box::<dyn StdError + Send + Sync>::from(text.to_owned());
            Root::Anyhow(anyhow::anyhow!(boxed))
        }
        _ => return None,
    })
}

/// Applies one context layer the way product code does: `.context(..)` on a
/// `Result` whose error is still the typed error when `direct` is set (the
/// `ContextError<C, E>` path), on an `anyhow::Error` otherwise.
fn layer(current: Root, spec: &Value) -> Option<Root> {
    let text = crate::text(spec, "text");
    let direct = spec.get("direct").and_then(Value::as_bool).unwrap_or(false);
    macro_rules! apply {
        ($context:expr) => {
            match current {
                Root::Std(StdRoot::Io(error)) if direct => Err::<(), _>(error).context($context).unwrap_err(),
                Root::Std(StdRoot::Typed(error)) if direct => {
                    Err::<(), _>(error).context($context).unwrap_err()
                }
                Root::Std(StdRoot::Io(error)) => Error::new(error).context($context),
                Root::Std(StdRoot::Typed(error)) => Error::new(error).context($context),
                Root::Anyhow(error) => Err::<(), Error>(error).context($context).unwrap_err(),
            }
        };
    }
    let error = match crate::text(spec, "kind") {
        "str" => apply!(leak(text)),
        "string" => apply!(text.to_owned()),
        "typed" => apply!(TypedCtx { text: text.to_owned() }),
        "int" => apply!(spec.get("value").and_then(Value::as_i64).unwrap_or_default() as i32),
        "error" => apply!(typed(&texts(spec))),
        _ => return None,
    };
    Some(Root::Anyhow(error))
}

fn into_error(root: Root) -> Error {
    match root {
        Root::Std(StdRoot::Io(error)) => Error::new(error),
        Root::Std(StdRoot::Typed(error)) => Error::from(error),
        Root::Anyhow(error) => error,
    }
}

fn text_or_null(text: Option<String>) -> Value {
    text.map_or(Value::Null, Value::String)
}

/// The forms built from each error's plain message, the chain, and what the
/// error downcasts to.
fn describe(error: &Error) -> Value {
    let mut downcast = Map::new();
    downcast.insert("str".into(), text_or_null(error.downcast_ref::<&'static str>().map(|v| (*v).to_owned())));
    downcast.insert("string".into(), text_or_null(error.downcast_ref::<String>().cloned()));
    downcast.insert("io".into(), text_or_null(error.downcast_ref::<io::Error>().map(ToString::to_string)));
    downcast.insert("typed".into(), text_or_null(error.downcast_ref::<Typed>().map(ToString::to_string)));
    downcast
        .insert("typed_ctx".into(), text_or_null(error.downcast_ref::<TypedCtx>().map(ToString::to_string)));
    downcast
        .insert("typed_msg".into(), text_or_null(error.downcast_ref::<TypedMsg>().map(ToString::to_string)));
    downcast.insert("int".into(), text_or_null(error.downcast_ref::<i32>().map(ToString::to_string)));
    downcast.insert(
        "boxed".into(),
        text_or_null(error.downcast_ref::<Box<dyn StdError + Send + Sync>>().map(ToString::to_string)),
    );

    // What `cause.downcast_ref::<T>()` finds on each element of the chain:
    // the `dyn Error` downcast, which sees only the element's own type, and
    // which Rust accepts only for a `T` that is itself an error.
    let chain_downcast: Vec<Value> = error
        .chain()
        .map(|cause| {
            let mut found = Vec::new();
            if cause.is::<io::Error>() {
                found.push("io");
            }
            if cause.is::<Typed>() {
                found.push("typed");
            }
            json!(found)
        })
        .collect();

    json!({
        "display": format!("{error}"),
        "alternate": format!("{error:#}"),
        "debug": format!("{error:?}"),
        "chain": error.chain().map(ToString::to_string).collect::<Vec<_>>(),
        "root_cause": error.root_cause().to_string(),
        "source": text_or_null(error.source().map(ToString::to_string)),
        "downcast": downcast,
        "chain_downcast": chain_downcast,
    })
}

/// The forms that depend on the formatter: the Debug form of the error object
/// and of each chain element, and what a width and a precision do.
fn describe_fmt(error: &Error) -> Value {
    json!({
        "display": format!("{error}"),
        "alternate": format!("{error:#}"),
        "debug": format!("{error:?}"),
        "alt_debug": format!("{error:#?}"),
        "width": format!("[{error:>40}|{error:<#40}|{error:^8.3}|{error:12?}]"),
        "chain_display": error.chain().map(|cause| format!("{cause}")).collect::<Vec<_>>(),
        "chain_debug": error.chain().map(|cause| format!("{cause:?}")).collect::<Vec<_>>(),
        "chain_alt_debug": error.chain().map(|cause| format!("{cause:#?}")).collect::<Vec<_>>(),
        "chain_width": error.chain().map(|cause| format!("[{cause:>12.5}]")).collect::<Vec<_>>(),
        "nested": format!("{:?}", Some(error)),
        "nested_alt": format!("{:#?}", (error, 1)),
    })
}

fn build(request: &Value) -> Option<Error> {
    let mut current = request.get("root").and_then(root)?;
    for spec in request.get("layers").and_then(Value::as_array).into_iter().flatten() {
        current = layer(current, spec)?;
    }
    Some(into_error(current))
}

pub fn handle(request: &Value) -> Value {
    match crate::text(request, "op") {
        "error" => build(request).map_or_else(|| crate::unknown(request), |error| describe(&error)),
        "error_fmt" => build(request).map_or_else(|| crate::unknown(request), |error| describe_fmt(&error)),
        "ensure" => ensure(request),
        _ => crate::unknown(request),
    }
}

/// The message of `ensure!(lhs == rhs)` when the comparison fails, for the
/// operand kinds the Go test generates. The condition text is fixed, so the
/// Go side passes the same text.
fn ensure(request: &Value) -> Value {
    fn failed(run: impl FnOnce() -> anyhow::Result<()>) -> Value {
        match run() {
            Ok(()) => Value::Null,
            Err(error) => Value::String(error.to_string()),
        }
    }
    let lhs = request.get("lhs").cloned().unwrap_or(Value::Null);
    let rhs = request.get("rhs").cloned().unwrap_or(Value::Null);
    let message = match crate::text(request, "type") {
        "i64" => {
            let (lhs, rhs) = (lhs.as_i64().unwrap_or_default(), rhs.as_i64().unwrap_or_default());
            failed(|| {
                anyhow::ensure!(lhs == rhs);
                Ok(())
            })
        }
        "string" => {
            let (lhs, rhs) =
                (lhs.as_str().unwrap_or_default().to_owned(), rhs.as_str().unwrap_or_default().to_owned());
            failed(|| {
                anyhow::ensure!(lhs == rhs);
                Ok(())
            })
        }
        "bool" => {
            let (lhs, rhs) = (lhs.as_bool().unwrap_or_default(), rhs.as_bool().unwrap_or_default());
            failed(|| {
                anyhow::ensure!(lhs == rhs);
                Ok(())
            })
        }
        "plain" => {
            let ok = lhs.as_bool().unwrap_or_default();
            failed(|| {
                anyhow::ensure!(ok);
                Ok(())
            })
        }
        _ => return crate::unknown(request),
    };
    json!({ "message": message })
}
