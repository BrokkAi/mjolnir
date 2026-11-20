//! Oracle module `clap`: runs clap 4.6.1 (features derive and env, as
//! Mjolnir builds it) on commands described in JSON, for the Go port in
//! `go/crates/clap` (`oracle_test.go`, `oracle_spec_test.go`).
//!
//! Requests (every one may carry `"env": {"NAME": "value"}`, set before the
//! command is built and removed afterwards):
//!
//! ```text
//! {"op": "run",    "cmd": CMD, "argv": [..]}     -> {"ok": MATCHES} | {"err": ERR}
//! {"op": "runs",   "cmd": CMD, "argvs": [[..]]}  -> {"results": [..]}, one fresh command each
//! {"op": "render", "cmd": CMD, "path": [..], "what": "help"|"long_help"|"usage"|"version"|"long_version"}
//! ```
//!
//! A CMD is `{"name": .., "calls": [[method, params..], ..]}`, the builder
//! calls in order; an ARG and a GROUP are `{"id": .., "calls": [..]}`. The
//! methods are the ones `cmd_call`, `arg_call` and `group` accept below.
//! MATCHES is `{"ids": [{"id", "source", "raw", "idx", "t", "v"}], "sub":
//! null | {"name", "m": MATCHES}}`; ERR is `{"kind", "stderr", "code",
//! "plain", "ansi", "context"}`.

use std::ffi::OsString;
use std::path::PathBuf;

use clap::builder::styling::{Ansi256Color, AnsiColor, RgbColor, Styles};
use clap::builder::{
    ArgPredicate, BoolishValueParser, FalseyValueParser, NonEmptyStringValueParser, PossibleValue,
    PossibleValuesParser, UnknownArgumentValueParser, ValueParser, ValueRange,
};
use clap::{Arg, ArgAction, ArgGroup, ArgMatches, ColorChoice, Command, Id, ValueEnum};
use serde_json::{Value, json};

type R<T> = Result<T, String>;

pub fn handle(request: &Value) -> Value {
    let _env = EnvGuard::set(request.get("env"));
    match dispatch(request) {
        Ok(reply) => reply,
        Err(error) => json!({ "oracle_error": error }),
    }
}

/// Sets the request's environment variables and removes them when dropped,
/// also when the code under test panics.
struct EnvGuard(Vec<String>);

impl EnvGuard {
    fn set(env: Option<&Value>) -> Self {
        let mut names = Vec::new();
        if let Some(env) = env.and_then(Value::as_object) {
            for (name, value) in env {
                if let Some(value) = value.as_str() {
                    // SAFETY: the oracle is single-threaded.
                    unsafe { std::env::set_var(name, value) };
                    names.push(name.clone());
                }
            }
        }
        Self(names)
    }
}

impl Drop for EnvGuard {
    fn drop(&mut self) {
        for name in &self.0 {
            // SAFETY: the oracle is single-threaded.
            unsafe { std::env::remove_var(name) };
        }
    }
}

fn dispatch(request: &Value) -> R<Value> {
    match crate::text(request, "op") {
        "run" => {
            let argv = strings(request.get("argv"))?;
            run_one(&request["cmd"], &argv)
        }
        "runs" => {
            let mut results = Vec::new();
            for argv in argvs(request)? {
                results.push(run_one(&request["cmd"], &argv)?);
            }
            Ok(json!({ "results": results }))
        }
        "render" => render(build_cmd(&request["cmd"])?, request),
        op @ ("mj_worker" | "mj_worker_runs" | "mj_worker_render") => derived::<mj_worker::Cli>(request, op),
        _ => Ok(crate::unknown(request)),
    }
}

fn argvs(request: &Value) -> R<Vec<Vec<String>>> {
    request
        .get("argvs")
        .and_then(Value::as_array)
        .ok_or("argvs is not an array")?
        .iter()
        .map(|argv| strings(Some(argv)))
        .collect()
}

fn run_one(spec: &Value, argv: &[String]) -> R<Value> {
    let mut cmd = build_cmd(spec)?;
    Ok(match cmd.try_get_matches_from_mut(argv) {
        Ok(matches) => json!({ "ok": dump_matches(&matches) }),
        Err(error) => json!({ "err": dump_error(&error) }),
    })
}

fn render(mut cmd: Command, request: &Value) -> R<Value> {
    let path = strings(request.get("path"))?;
    let what = crate::text(request, "what");
    if path.is_empty() {
        return render_one(&mut cmd, what);
    }
    cmd.build();
    let mut current = &mut cmd;
    for name in &path {
        current = current
            .find_subcommand_mut(name)
            .ok_or_else(|| format!("no subcommand {name}"))?;
    }
    render_one(current, what)
}

fn render_one(cmd: &mut Command, what: &str) -> R<Value> {
    let styled = |s: clap::builder::StyledStr| json!({ "plain": s.to_string(), "ansi": s.ansi().to_string() });
    Ok(match what {
        "help" => styled(cmd.render_help()),
        "long_help" => styled(cmd.render_long_help()),
        "usage" => styled(cmd.render_usage()),
        "version" => json!({ "plain": cmd.render_version() }),
        "long_version" => json!({ "plain": cmd.render_long_version() }),
        _ => return Err(format!("unknown render {what}")),
    })
}

pub(crate) fn dump_error(error: &clap::Error) -> Value {
    let rendered = error.render();
    let context: Vec<Value> = error
        .context()
        .map(|(kind, value)| json!([format!("{kind:?}"), value.to_string()]))
        .collect();
    json!({
        "kind": format!("{:?}", error.kind()),
        "stderr": error.use_stderr(),
        "code": error.exit_code(),
        "plain": rendered.to_string(),
        "ansi": rendered.ansi().to_string(),
        "context": context,
    })
}

fn dump_matches(matches: &ArgMatches) -> Value {
    let ids: Vec<Value> = matches.ids().map(|id| dump_arg(matches, id.as_str())).collect();
    let sub = match matches.subcommand() {
        Some((name, sub)) => json!({ "name": name, "m": dump_matches(sub) }),
        None => Value::Null,
    };
    json!({ "ids": ids, "sub": sub })
}

fn dump_arg(matches: &ArgMatches, id: &str) -> Value {
    let source = matches.value_source(id).map(|source| format!("{source:?}"));
    let raw: Option<Vec<Vec<String>>> = matches.get_raw_occurrences(id).map(|occurrences| {
        occurrences
            .map(|values| values.map(|v| v.to_string_lossy().into_owned()).collect())
            .collect()
    });
    let idx: Option<Vec<usize>> = matches.indices_of(id).map(Iterator::collect);
    let (tag, values) = typed(matches, id);
    json!({ "id": id, "source": source, "raw": raw, "idx": idx, "t": tag, "v": values })
}

macro_rules! try_type {
    ($matches:expr, $id:expr, $ty:ty, $tag:expr, $convert:expr) => {
        match $matches.try_get_occurrences::<$ty>($id) {
            Ok(Some(occurrences)) => {
                let convert = $convert;
                let values: Vec<Value> = occurrences
                    .map(|values| Value::Array(values.map(|v| convert(v)).collect()))
                    .collect();
                return (json!($tag), Value::Array(values));
            }
            Ok(None) => return (Value::Null, Value::Null),
            Err(_) => {}
        }
    };
}

/// The values of an argument as the first of the protocol's types that
/// fits.
fn typed(matches: &ArgMatches, id: &str) -> (Value, Value) {
    try_type!(matches, id, String, "str", |v: &String| json!(v));
    try_type!(matches, id, OsString, "str", |v: &OsString| json!(v.to_string_lossy()));
    try_type!(matches, id, PathBuf, "str", |v: &PathBuf| json!(v.to_string_lossy()));
    try_type!(matches, id, bool, "bool", |v: &bool| json!(v));
    try_type!(matches, id, u8, "u8", |v: &u8| json!(v));
    try_type!(matches, id, i8, "i8", |v: &i8| json!(v));
    try_type!(matches, id, u16, "u16", |v: &u16| json!(v));
    try_type!(matches, id, i16, "i16", |v: &i16| json!(v));
    try_type!(matches, id, u32, "u32", |v: &u32| json!(v));
    try_type!(matches, id, i32, "i32", |v: &i32| json!(v));
    try_type!(matches, id, u64, "u64", |v: &u64| json!(v));
    try_type!(matches, id, i64, "i64", |v: &i64| json!(v));
    try_type!(matches, id, usize, "size", |v: &usize| json!(v));
    try_type!(matches, id, isize, "size", |v: &isize| json!(v));
    try_type!(matches, id, f64, "f64", |v: &f64| json!(format!("{v}")));
    try_type!(matches, id, f32, "f32", |v: &f32| json!(format!("{v}")));
    try_type!(matches, id, Id, "id", |v: &Id| json!(v.as_str()));
    try_type!(matches, id, ColorChoice, "color", |v: &ColorChoice| json!(v.to_string()));
    try_type!(matches, id, Mode, "mode", |v: &Mode| json!(v.to_possible_value().unwrap().get_name()));
    (Value::Null, Value::Null)
}

/// The protocol's test enum.
#[derive(Clone, Copy, Debug, PartialEq, Eq, clap::ValueEnum)]
enum Mode {
    /// Go quickly
    Fast,
    #[value(alias = "sl", help = "Take your time")]
    Slow,
    #[value(name = "very-careful")]
    VeryCareful,
    #[value(skip)]
    #[allow(dead_code)]
    Unused,
    #[value(hide = true)]
    Secret,
}

/// clap without its `string` feature takes names only as `&'static str`.
fn leak(s: &str) -> &'static str {
    Box::leak(s.to_owned().into_boxed_str())
}

fn strings(value: Option<&Value>) -> R<Vec<String>> {
    value
        .and_then(Value::as_array)
        .ok_or_else(|| format!("not an array of strings: {value:?}"))?
        .iter()
        .map(|v| v.as_str().map(str::to_owned).ok_or_else(|| format!("not a string: {v}")))
        .collect()
}

fn calls(spec: &Value) -> R<Vec<(String, Vec<Value>)>> {
    let calls = match spec.get("calls") {
        // A spec with no calls may leave them out or write null.
        None | Some(Value::Null) => return Ok(Vec::new()),
        Some(calls) => calls.as_array().ok_or_else(|| format!("calls are not an array in {spec}"))?,
    };
    calls
        .iter()
        .map(|call| {
            let parts = call.as_array().ok_or_else(|| format!("call is not an array: {call}"))?;
            let method = parts
                .first()
                .and_then(Value::as_str)
                .ok_or_else(|| format!("call has no method: {call}"))?;
            Ok((method.to_owned(), parts[1..].to_vec()))
        })
        .collect()
}

fn p_str(params: &[Value], i: usize) -> R<&'static str> {
    params
        .get(i)
        .and_then(Value::as_str)
        .map(leak)
        .ok_or_else(|| format!("parameter {i} is not a string: {params:?}"))
}

fn p_opt_str(params: &[Value], i: usize) -> R<Option<&'static str>> {
    match params.get(i) {
        Some(Value::Null) => Ok(None),
        _ => p_str(params, i).map(Some),
    }
}

fn p_strs(params: &[Value], i: usize) -> R<Vec<&'static str>> {
    Ok(strings(params.get(i))?.iter().map(|s| leak(s)).collect())
}

fn p_bool(params: &[Value], i: usize) -> R<bool> {
    params
        .get(i)
        .and_then(Value::as_bool)
        .ok_or_else(|| format!("parameter {i} is not a bool: {params:?}"))
}

fn p_num(params: &[Value], i: usize) -> R<usize> {
    params
        .get(i)
        .and_then(Value::as_u64)
        .map(|n| n as usize)
        .ok_or_else(|| format!("parameter {i} is not a number: {params:?}"))
}

fn p_opt_num(params: &[Value], i: usize) -> R<Option<usize>> {
    match params.get(i) {
        Some(Value::Null) => Ok(None),
        _ => p_num(params, i).map(Some),
    }
}

fn p_char(params: &[Value], i: usize) -> R<char> {
    let s = p_str(params, i)?;
    let mut chars = s.chars();
    match (chars.next(), chars.next()) {
        (Some(c), None) => Ok(c),
        _ => Err(format!("not one character: {s:?}")),
    }
}

pub(crate) fn build_cmd(spec: &Value) -> R<Command> {
    let name = spec
        .get("name")
        .and_then(Value::as_str)
        .ok_or_else(|| format!("command has no name: {spec}"))?;
    let mut cmd = Command::new(leak(name));
    for (method, params) in calls(spec)? {
        cmd = cmd_call(cmd, &method, &params)?;
    }
    Ok(cmd)
}

fn custom_styles() -> Styles {
    Styles::styled()
        .header(AnsiColor::Yellow.on_default().bold().underline())
        .usage(AnsiColor::Green.on_default().bold())
        .literal(AnsiColor::Cyan.on_default().bold())
        .placeholder(AnsiColor::Cyan.on_default())
        .error(AnsiColor::Red.on_default().bold())
        .valid(AnsiColor::Green.on_default())
        .invalid(AnsiColor::Yellow.on_default().bold())
        .context(RgbColor(10, 20, 30).on_default().dimmed())
        .context_value(Ansi256Color(208).on_default().italic())
}

fn cmd_call(cmd: Command, method: &str, params: &[Value]) -> R<Command> {
    Ok(match method {
        "about" => cmd.about(p_str(params, 0)?),
        "long_about" => cmd.long_about(p_str(params, 0)?),
        "version" => cmd.version(p_str(params, 0)?),
        "long_version" => cmd.long_version(p_str(params, 0)?),
        "author" => cmd.author(p_str(params, 0)?),
        "bin_name" => cmd.bin_name(p_str(params, 0)?),
        "display_name" => cmd.display_name(p_str(params, 0)?),
        "before_help" => cmd.before_help(p_str(params, 0)?),
        "before_long_help" => cmd.before_long_help(p_str(params, 0)?),
        "after_help" => cmd.after_help(p_str(params, 0)?),
        "after_long_help" => cmd.after_long_help(p_str(params, 0)?),
        "override_usage" => cmd.override_usage(p_str(params, 0)?),
        "override_help" => cmd.override_help(p_str(params, 0)?),
        "help_template" => cmd.help_template(p_str(params, 0)?),
        "subcommand_value_name" => cmd.subcommand_value_name(p_str(params, 0)?),
        "subcommand_help_heading" => cmd.subcommand_help_heading(p_str(params, 0)?),
        "alias" => cmd.alias(p_str(params, 0)?),
        "visible_alias" => cmd.visible_alias(p_str(params, 0)?),
        "long_flag" => cmd.long_flag(p_str(params, 0)?),
        "long_flag_alias" => cmd.long_flag_alias(p_str(params, 0)?),
        "visible_long_flag_alias" => cmd.visible_long_flag_alias(p_str(params, 0)?),
        "aliases" => cmd.aliases(p_strs(params, 0)?),
        "visible_aliases" => cmd.visible_aliases(p_strs(params, 0)?),
        "short_flag" => cmd.short_flag(p_char(params, 0)?),
        "short_flag_alias" => cmd.short_flag_alias(p_char(params, 0)?),
        "visible_short_flag_alias" => cmd.visible_short_flag_alias(p_char(params, 0)?),
        "next_help_heading" => cmd.next_help_heading(p_opt_str(params, 0)?),
        "next_display_order" => cmd.next_display_order(p_opt_num(params, 0)?),
        "display_order" => cmd.display_order(p_num(params, 0)?),
        "term_width" => cmd.term_width(p_num(params, 0)?),
        "max_term_width" => cmd.max_term_width(p_num(params, 0)?),
        "color" => cmd.color(match p_str(params, 0)? {
            "auto" => ColorChoice::Auto,
            "always" => ColorChoice::Always,
            "never" => ColorChoice::Never,
            other => return Err(format!("unknown color {other}")),
        }),
        "styles" => cmd.styles(match p_str(params, 0)? {
            "plain" => Styles::plain(),
            "styled" => Styles::styled(),
            "custom" => custom_styles(),
            other => return Err(format!("unknown styles {other}")),
        }),
        "no_binary_name" => cmd.no_binary_name(p_bool(params, 0)?),
        "ignore_errors" => cmd.ignore_errors(p_bool(params, 0)?),
        "args_override_self" => cmd.args_override_self(p_bool(params, 0)?),
        "dont_delimit_trailing_values" => cmd.dont_delimit_trailing_values(p_bool(params, 0)?),
        "propagate_version" => cmd.propagate_version(p_bool(params, 0)?),
        "next_line_help" => cmd.next_line_help(p_bool(params, 0)?),
        "disable_help_flag" => cmd.disable_help_flag(p_bool(params, 0)?),
        "disable_version_flag" => cmd.disable_version_flag(p_bool(params, 0)?),
        "disable_help_subcommand" => cmd.disable_help_subcommand(p_bool(params, 0)?),
        "disable_colored_help" => cmd.disable_colored_help(p_bool(params, 0)?),
        "help_expected" => cmd.help_expected(p_bool(params, 0)?),
        "hide_possible_values" => cmd.hide_possible_values(p_bool(params, 0)?),
        "infer_long_args" => cmd.infer_long_args(p_bool(params, 0)?),
        "infer_subcommands" => cmd.infer_subcommands(p_bool(params, 0)?),
        "arg_required_else_help" => cmd.arg_required_else_help(p_bool(params, 0)?),
        "allow_missing_positional" => cmd.allow_missing_positional(p_bool(params, 0)?),
        "hide" => cmd.hide(p_bool(params, 0)?),
        "subcommand_required" => cmd.subcommand_required(p_bool(params, 0)?),
        "allow_external_subcommands" => cmd.allow_external_subcommands(p_bool(params, 0)?),
        "args_conflicts_with_subcommands" => cmd.args_conflicts_with_subcommands(p_bool(params, 0)?),
        "subcommand_precedence_over_arg" => cmd.subcommand_precedence_over_arg(p_bool(params, 0)?),
        "subcommand_negates_reqs" => cmd.subcommand_negates_reqs(p_bool(params, 0)?),
        "flatten_help" => cmd.flatten_help(p_bool(params, 0)?),
        "multicall" => cmd.multicall(p_bool(params, 0)?),
        "external_subcommand_value_parser" => {
            cmd.external_subcommand_value_parser(value_parser(params.first().unwrap_or(&Value::Null))?)
        }
        "arg" => cmd.arg(build_arg(params.first().unwrap_or(&Value::Null))?),
        "group" => cmd.group(build_group(params.first().unwrap_or(&Value::Null))?),
        "subcommand" => cmd.subcommand(build_cmd(params.first().unwrap_or(&Value::Null))?),
        "mut_arg" => {
            let id = p_str(params, 0)?;
            let inner = params.get(1).cloned().unwrap_or(Value::Null);
            let inner = calls(&json!({ "calls": inner }))?;
            let mut failure = None;
            let cmd = cmd.mut_arg(id, |mut arg| {
                for (method, params) in &inner {
                    match arg_call(arg.clone(), method, params) {
                        Ok(next) => arg = next,
                        Err(error) => failure = Some(error),
                    }
                }
                arg
            });
            if let Some(error) = failure {
                return Err(error);
            }
            cmd
        }
        "mut_subcommand" => {
            let name = p_str(params, 0)?;
            let inner = params.get(1).cloned().unwrap_or(Value::Null);
            let inner = calls(&json!({ "calls": inner }))?;
            let mut failure = None;
            let cmd = cmd.mut_subcommand(name, |mut sub| {
                for (method, params) in &inner {
                    match cmd_call(sub.clone(), method, params) {
                        Ok(next) => sub = next,
                        Err(error) => failure = Some(error),
                    }
                }
                sub
            });
            if let Some(error) = failure {
                return Err(error);
            }
            cmd
        }
        other => return Err(format!("unknown command method {other}")),
    })
}

fn build_arg(spec: &Value) -> R<Arg> {
    let id = spec
        .get("id")
        .and_then(Value::as_str)
        .ok_or_else(|| format!("arg has no id: {spec}"))?;
    let mut arg = Arg::new(leak(id));
    for (method, params) in calls(spec)? {
        arg = arg_call(arg, &method, &params)?;
    }
    Ok(arg)
}

fn predicate(value: &Value) -> R<ArgPredicate> {
    match value {
        Value::String(s) if s == "present" => Ok(ArgPredicate::IsPresent),
        Value::Object(map) => match map.get("equals").and_then(Value::as_str) {
            Some(s) => Ok(ArgPredicate::Equals(leak(s).into())),
            None => Err(format!("bad predicate {value}")),
        },
        _ => Err(format!("bad predicate {value}")),
    }
}

fn pairs(value: Option<&Value>) -> R<Vec<(&'static str, &'static str)>> {
    value
        .and_then(Value::as_array)
        .ok_or("pairs are not an array")?
        .iter()
        .map(|pair| {
            let pair = strings(Some(pair))?;
            match pair.as_slice() {
                [a, b] => Ok((leak(a), leak(b))),
                _ => Err(format!("not a pair: {pair:?}")),
            }
        })
        .collect()
}

fn arg_call(arg: Arg, method: &str, params: &[Value]) -> R<Arg> {
    Ok(match method {
        "short" => arg.short(p_char(params, 0)?),
        "long" => arg.long(p_str(params, 0)?),
        "alias" => arg.alias(p_str(params, 0)?),
        "visible_alias" => arg.visible_alias(p_str(params, 0)?),
        "aliases" => arg.aliases(p_strs(params, 0)?),
        "visible_aliases" => arg.visible_aliases(p_strs(params, 0)?),
        "short_alias" => arg.short_alias(p_char(params, 0)?),
        "visible_short_alias" => arg.visible_short_alias(p_char(params, 0)?),
        "index" => arg.index(p_num(params, 0)?),
        "value_name" => arg.value_name(p_str(params, 0)?),
        "value_names" => arg.value_names(p_strs(params, 0)?),
        "value_delimiter" => arg.value_delimiter(p_char(params, 0)?),
        "value_terminator" => arg.value_terminator(p_str(params, 0)?),
        "default_value" => arg.default_value(p_str(params, 0)?),
        "default_values" => arg.default_values(p_strs(params, 0)?),
        "default_missing_value" => arg.default_missing_value(p_str(params, 0)?),
        "default_missing_values" => arg.default_missing_values(p_strs(params, 0)?),
        "env" => arg.env(p_str(params, 0)?),
        "help" => arg.help(p_str(params, 0)?),
        "long_help" => arg.long_help(p_str(params, 0)?),
        "display_order" => arg.display_order(p_num(params, 0)?),
        "help_heading" => arg.help_heading(p_opt_str(params, 0)?),
        "requires" => arg.requires(p_str(params, 0)?),
        "group" => arg.group(p_str(params, 0)?),
        "groups" => arg.groups(p_strs(params, 0)?),
        "required_unless_present" => arg.required_unless_present(p_str(params, 0)?),
        "required_unless_present_all" => arg.required_unless_present_all(p_strs(params, 0)?),
        "required_unless_present_any" => arg.required_unless_present_any(p_strs(params, 0)?),
        "required_if_eq" => arg.required_if_eq(p_str(params, 0)?, p_str(params, 1)?),
        "required_if_eq_any" => arg.required_if_eq_any(pairs(params.first())?),
        "required_if_eq_all" => arg.required_if_eq_all(pairs(params.first())?),
        "conflicts_with" => arg.conflicts_with(p_str(params, 0)?),
        "conflicts_with_all" => arg.conflicts_with_all(p_strs(params, 0)?),
        "overrides_with" => arg.overrides_with(p_str(params, 0)?),
        "overrides_with_all" => arg.overrides_with_all(p_strs(params, 0)?),
        "trailing_var_arg" => arg.trailing_var_arg(p_bool(params, 0)?),
        "last" => arg.last(p_bool(params, 0)?),
        "required" => arg.required(p_bool(params, 0)?),
        "exclusive" => arg.exclusive(p_bool(params, 0)?),
        "global" => arg.global(p_bool(params, 0)?),
        "ignore_case" => arg.ignore_case(p_bool(params, 0)?),
        "allow_hyphen_values" => arg.allow_hyphen_values(p_bool(params, 0)?),
        "allow_negative_numbers" => arg.allow_negative_numbers(p_bool(params, 0)?),
        "require_equals" => arg.require_equals(p_bool(params, 0)?),
        "raw" => arg.raw(p_bool(params, 0)?),
        "next_line_help" => arg.next_line_help(p_bool(params, 0)?),
        "hide" => arg.hide(p_bool(params, 0)?),
        "hide_possible_values" => arg.hide_possible_values(p_bool(params, 0)?),
        "hide_default_value" => arg.hide_default_value(p_bool(params, 0)?),
        "hide_env" => arg.hide_env(p_bool(params, 0)?),
        "hide_env_values" => arg.hide_env_values(p_bool(params, 0)?),
        "hide_short_help" => arg.hide_short_help(p_bool(params, 0)?),
        "hide_long_help" => arg.hide_long_help(p_bool(params, 0)?),
        "action" => arg.action(match p_str(params, 0)? {
            "set" => ArgAction::Set,
            "append" => ArgAction::Append,
            "set_true" => ArgAction::SetTrue,
            "set_false" => ArgAction::SetFalse,
            "count" => ArgAction::Count,
            "help" => ArgAction::Help,
            "help_short" => ArgAction::HelpShort,
            "help_long" => ArgAction::HelpLong,
            "version" => ArgAction::Version,
            other => return Err(format!("unknown action {other}")),
        }),
        "num_args" => arg.num_args(value_range(params.first().unwrap_or(&Value::Null))?),
        "value_parser" => arg.value_parser(value_parser(params.first().unwrap_or(&Value::Null))?),
        "default_value_if" => arg.default_value_if(
            p_str(params, 0)?,
            predicate(params.get(1).unwrap_or(&Value::Null))?,
            p_opt_str(params, 2)?,
        ),
        "requires_if" => arg.requires_if(p_str(params, 0)?, p_str(params, 1)?),
        other => return Err(format!("unknown arg method {other}")),
    })
}

fn value_range(value: &Value) -> R<ValueRange> {
    if let Some(n) = value.as_u64() {
        return Ok((n as usize).into());
    }
    let bounds = value.as_array().ok_or_else(|| format!("bad num_args {value}"))?;
    match bounds.as_slice() {
        [Value::Number(lo), Value::Null] => Ok((lo.as_u64().ok_or("bad bound")? as usize..).into()),
        [Value::Number(lo), Value::Number(hi)] => {
            let lo = lo.as_u64().ok_or("bad bound")? as usize;
            let hi = hi.as_u64().ok_or("bad bound")? as usize;
            Ok((lo..=hi).into())
        }
        _ => Err(format!("bad num_args {value}")),
    }
}

fn even(s: &str) -> Result<i64, String> {
    let n: i64 = s.parse().map_err(|e: std::num::ParseIntError| e.to_string())?;
    if n % 2 == 0 { Ok(n) } else { Err(format!("{n} is odd")) }
}

fn upper(s: &str) -> Result<String, String> {
    if s.is_empty() {
        Err("empty value".to_owned())
    } else {
        Ok(s.to_ascii_uppercase())
    }
}

fn value_parser(value: &Value) -> R<ValueParser> {
    if let Some(name) = value.as_str() {
        return Ok(match name {
            "string" => clap::value_parser!(String).into(),
            "os_string" => clap::value_parser!(OsString).into(),
            "path" => clap::value_parser!(PathBuf).into(),
            "bool" => clap::value_parser!(bool).into(),
            "boolish" => BoolishValueParser::new().into(),
            "falsey" => FalseyValueParser::new().into(),
            "non_empty_string" => NonEmptyStringValueParser::new().into(),
            "u8" => clap::value_parser!(u8).into(),
            "i8" => clap::value_parser!(i8).into(),
            "u16" => clap::value_parser!(u16).into(),
            "i16" => clap::value_parser!(i16).into(),
            "u32" => clap::value_parser!(u32).into(),
            "i32" => clap::value_parser!(i32).into(),
            "u64" => clap::value_parser!(u64).into(),
            "i64" => clap::value_parser!(i64).into(),
            "usize" => clap::value_parser!(usize).into(),
            "isize" => clap::value_parser!(isize).into(),
            "f64" => clap::value_parser!(f64).into(),
            "f32" => clap::value_parser!(f32).into(),
            "color" => clap::value_parser!(ColorChoice).into(),
            "mode" => clap::value_parser!(Mode).into(),
            "fn_even" => ValueParser::new(even),
            "fn_upper" => ValueParser::new(upper),
            other => return Err(format!("unknown value parser {other}")),
        });
    }
    let object = value.as_object().ok_or_else(|| format!("bad value parser {value}"))?;
    if let Some(values) = object.get("possible").and_then(Value::as_array) {
        let mut possible = Vec::new();
        for pv in values {
            if let Some(name) = pv.as_str() {
                possible.push(PossibleValue::new(leak(name)));
                continue;
            }
            let name = pv.get("name").and_then(Value::as_str).ok_or("possible value without name")?;
            let mut value = PossibleValue::new(leak(name));
            if let Some(help) = pv.get("help").and_then(Value::as_str) {
                value = value.help(leak(help));
            }
            if let Some(hide) = pv.get("hide").and_then(Value::as_bool) {
                value = value.hide(hide);
            }
            if pv.get("aliases").is_some() {
                value = value.aliases(strings(pv.get("aliases"))?.iter().map(|s| leak(s)));
            }
            possible.push(value);
        }
        return Ok(PossibleValuesParser::new(possible).into());
    }
    if let Some(bounds) = object.get("i64_range").and_then(Value::as_array) {
        let lo = bounds.first().and_then(Value::as_i64);
        let hi = bounds.get(1).and_then(Value::as_i64);
        let parser = clap::value_parser!(i64);
        return Ok(match (lo, hi) {
            (Some(lo), Some(hi)) => parser.range(lo..=hi).into(),
            (Some(lo), None) => parser.range(lo..).into(),
            (None, Some(hi)) => parser.range(..=hi).into(),
            (None, None) => parser.range(..).into(),
        });
    }
    if let Some(bounds) = object.get("u64_range").and_then(Value::as_array) {
        let lo = bounds.first().and_then(Value::as_u64);
        let hi = bounds.get(1).and_then(Value::as_u64);
        let parser = clap::value_parser!(u64);
        return Ok(match (lo, hi) {
            (Some(lo), Some(hi)) => parser.range(lo..=hi).into(),
            (Some(lo), None) => parser.range(lo..).into(),
            (None, Some(hi)) => parser.range(..=hi).into(),
            (None, None) => parser.range(..).into(),
        });
    }
    if let Some(arg) = object.get("unknown_suggest").and_then(Value::as_str) {
        return Ok(UnknownArgumentValueParser::suggest_arg(leak(arg)).into());
    }
    Err(format!("unknown value parser {value}"))
}

fn build_group(spec: &Value) -> R<ArgGroup> {
    let id = spec
        .get("id")
        .and_then(Value::as_str)
        .ok_or_else(|| format!("group has no id: {spec}"))?;
    let mut group = ArgGroup::new(leak(id));
    for (method, params) in calls(spec)? {
        group = match method.as_str() {
            "arg" => group.arg(p_str(&params, 0)?),
            "args" => group.args(p_strs(&params, 0)?),
            "required" => group.required(p_bool(&params, 0)?),
            "multiple" => group.multiple(p_bool(&params, 0)?),
            "requires" => group.requires(p_str(&params, 0)?),
            "requires_all" => group.requires_all(p_strs(&params, 0)?),
            "conflicts_with" => group.conflicts_with(p_str(&params, 0)?),
            "conflicts_with_all" => group.conflicts_with_all(p_strs(&params, 0)?),
            other => return Err(format!("unknown group method {other}")),
        };
    }
    Ok(group)
}

/// The command line of mj-worker, copied from mj-worker-bin/src/main.rs
/// (the clap items, verbatim but for `Serialize` and the stand-ins for
/// mj_core's types below).
#[allow(dead_code)]
mod mj_worker {
    use std::path::PathBuf;

    use clap::{Args, Parser, Subcommand};
    use serde::Serialize;

    use super::{HarnessKind, SubagentMcpRole};

    #[derive(Debug, Parser, Serialize)]
    #[command(name = "mj-worker", version, about = "Mjolnir target-side worker")]
    pub(super) struct Cli {
        /// Internal handoff: the parent explicitly supplied the clean login snapshot.
        #[arg(long, hide = true, global = true)]
        login_environment_ready: bool,
        /// Descriptor transferred by login-environment re-exec.
        #[arg(long, hide = true, global = true)]
        worker_root_lock_fd: Option<i32>,
        #[command(subcommand)]
        command: Command,
    }

    #[derive(Debug, Subcommand, Serialize)]
    enum Command {
        /// Internal target-side worker commands.
        #[command(hide = true)]
        Worker(WorkerArgs),
    }

    #[derive(Debug, Args, Serialize)]
    struct WorkerArgs {
        #[command(subcommand)]
        command: WorkerCommand,
    }

    #[derive(Debug, Subcommand, Serialize)]
    enum WorkerCommand {
        /// Move-only inventory and disk-backed workspace transfer utilities.
        #[command(hide = true)]
        MoveWorkspace,
        /// Print `alive`, `starting` or `dead` for the worker that owns a root,
        /// decided by the lock on its `worker.lock`.
        #[command(hide = true)]
        Liveness {
            #[arg(long)]
            root: PathBuf,
        },
        /// Print the pid of the worker holding a root's lock, or nothing.
        #[command(hide = true)]
        Holder {
            #[arg(long)]
            root: PathBuf,
        },
        /// Stop the worker that owns a root and every process it started, and
        /// return only when its lock is free and none of them is left.
        #[command(hide = true)]
        Stop {
            #[arg(long)]
            root: PathBuf,
            /// Kill at once instead of allowing the TERM grace.
            #[arg(long)]
            kill: bool,
        },
        /// Print this worker's compatibility identity. The controller asks an
        /// image's worker this before linking a session to it.
        #[command(hide = true)]
        BuildId,
        /// Discover profile model choices without submitting a prompt.
        DiscoverConfig {
            #[arg(long)]
            spec: PathBuf,
        },
        /// Own an ACP bridge and durable session event log.
        Run {
            #[arg(long)]
            root: PathBuf,
            #[arg(long)]
            config: PathBuf,
        },
        /// Prepare an exact managed harness without starting a session worker.
        PrepareHarness {
            #[arg(long)]
            config: PathBuf,
        },
        /// Proxy JSON-lines between stdio and a detached worker.
        Proxy {
            #[arg(long)]
            root: PathBuf,
        },
        /// Supervise the ACP bridge process tree for a worker daemon.
        AcpSupervisor {
            #[arg(long)]
            spec: PathBuf,
        },
        /// Build a target-side archive for verified controller transfer.
        ExportCheckpoint {
            /// Export specification path, or `-` to read it from standard input.
            #[arg(long)]
            spec: PathBuf,
        },
        /// Seal target-owned checkpoint inputs while ACP dispatch is frozen.
        #[command(hide = true)]
        CaptureCheckpoint,
        /// Package a sealed checkpoint after ACP dispatch has resumed.
        #[command(hide = true)]
        PackCheckpoint,
        /// Restore a verified archive into a freshly cloned target.
        RestoreCheckpoint {
            #[arg(long)]
            spec: PathBuf,
            /// Restore only missing parent state; never modify checkout or child reports.
            #[arg(long, conflicts_with = "missing_checkout")]
            missing_parent: bool,
            #[arg(long)]
            missing_checkout: bool,
            /// Current controller-owned queue/frontier, for partial parent recovery.
            #[arg(long, requires = "missing_parent")]
            relay_seed: Option<PathBuf>,
        },
        /// Journal queue discard while the worker is stopped, before any dispatch.
        #[command(hide = true)]
        DiscardQueuedPrompts {
            #[arg(long)]
            root: PathBuf,
            #[arg(long)]
            session_id: String,
            #[arg(long)]
            command_id: String,
        },
        /// Install one streamed resource directory on a remote target.
        InstallResource {
            #[arg(long)]
            destination: PathBuf,
        },
        /// Serve project history tools over MCP stdio.
        MemoryMcp {
            /// Accepted for staged Kimi profiles created by older controllers.
            #[arg(long, hide = true)]
            _root: Option<PathBuf>,
            #[arg(long)]
            history_socket: Option<PathBuf>,
        },
        /// Drain mailbox events for one harness tool hook.
        MailboxHook {
            #[arg(long)]
            socket: PathBuf,
            #[arg(long, value_parser = ["PostToolUse", "PostToolBatch"])]
            event: String,
        },
        /// Exchange one mailbox lease request for a durable harness inbox.
        MailboxRequest {
            #[arg(long)]
            socket: PathBuf,
        },
        /// Serve Mjolnir-owned delegation tools over MCP stdio.
        SubagentMcp {
            #[arg(long)]
            socket: PathBuf,
            /// Harness used to choose the MCP server instructions.
            #[arg(long)]
            harness: Option<HarnessKind>,
            /// `parent` serves the delegation tools; `child` serves only
            /// `handback`, a child's report to the session that started it.
            #[arg(long, default_value_t = SubagentMcpRole::Parent)]
            role: SubagentMcpRole,
            /// Accepted for older launch arguments; tool availability is unchanged.
            #[arg(long, default_value_t = true, action = clap::ArgAction::Set)]
            agent_mailboxes_enabled: bool,
            /// False when only the straightforward task type is configured: the
            /// spawn tool then offers no task type.
            #[arg(long, default_value_t = true, action = clap::ArgAction::Set)]
            subtle_subagents: bool,
            /// Accepted for configurations written by older workers; the review
            /// tools now belong to the `--review-server` server.
            #[arg(long, hide = true)]
            review_gate: bool,
            /// Serve the before-push review tools as `mj-review` instead of the
            /// delegation tools.
            #[arg(long)]
            review_server: bool,
        },
        /// Print a unified diff of the session's work in one repository.
        Diff {
            #[arg(long)]
            repository: PathBuf,
            /// The commit the session started from, when the controller recorded one.
            #[arg(long)]
            base: Option<String>,
            /// The session branch, whose reflog names the commit it was created at.
            #[arg(long)]
            branch: Option<String>,
            /// Include the resolved base and HEAD commit in JSON output.
            #[arg(long)]
            json: bool,
        },
        /// Write one file from the session workspace to standard output.
        ReadFile {
            #[arg(long)]
            root: PathBuf,
            /// Path relative to the workspace root.
            #[arg(long)]
            path: PathBuf,
        },
        /// Write a file, a zip archive of a directory, or a JSON listing of either
        /// to standard output, after one JSON line naming which it is and where
        /// it resolved.
        Fetch {
            /// The file or directory to fetch.
            #[arg(long)]
            path: PathBuf,
            /// A directory the path may resolve inside; repeatable.
            #[arg(long = "root")]
            roots: Vec<PathBuf>,
            /// Also allow this machine's temporary directory.
            #[arg(long)]
            temp: bool,
            /// List the path instead of reading it.
            #[arg(long)]
            list: bool,
        },
        /// Save fetched bytes in this target's temporary directory.
        StoreFetch {
            #[arg(long)]
            session: String,
            #[arg(long)]
            source: String,
            #[arg(long)]
            directory: bool,
            #[arg(long)]
            length: usize,
        },
        /// Atomically publish stdin as a file in the session workspace.
        WriteFile {
            #[arg(long)]
            root: PathBuf,
            #[arg(long)]
            path: PathBuf,
            #[arg(long)]
            overwrite: bool,
            /// Expected input length; premature EOF must not publish a partial file.
            #[arg(long)]
            length: usize,
        },
        /// Push the repository's current HEAD to a branch on its push remote.
        PushBranch {
            /// Worker root containing the session's existing Git authentication.
            #[arg(long)]
            root: PathBuf,
            #[arg(long)]
            repository: PathBuf,
            #[arg(long)]
            branch: String,
        },
    }
}

/// mj_core::config::HarnessKind as clap sees it: its FromStr and its ids.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum HarnessKind {
    Codex,
    Claude,
    Kimi,
    Grok,
    Muse,
    OpenCode,
    Dsh,
    Antigravity,
}

impl HarnessKind {
    const ALL: [Self; 8] = [
        Self::Codex,
        Self::Claude,
        Self::Kimi,
        Self::Grok,
        Self::Muse,
        Self::OpenCode,
        Self::Dsh,
        Self::Antigravity,
    ];

    const fn id(self) -> &'static str {
        match self {
            Self::Codex => "codex",
            Self::Claude => "claude",
            Self::Kimi => "kimi",
            Self::Grok => "grok",
            Self::Muse => "muse",
            Self::OpenCode => "opencode",
            Self::Dsh => "dsh",
            Self::Antigravity => "antigravity",
        }
    }
}

impl std::str::FromStr for HarnessKind {
    type Err = String;

    /// The real error is `anyhow!("unknown harness kind {value:?}")`, whose
    /// Display is this text.
    fn from_str(value: &str) -> Result<Self, String> {
        Self::ALL
            .into_iter()
            .find(|kind| kind.id() == value)
            .ok_or_else(|| format!("unknown harness kind {value:?}"))
    }
}

impl serde::Serialize for HarnessKind {
    fn serialize<S: serde::Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        serializer.serialize_str(self.id())
    }
}

/// mj_core::subagent::SubagentMcpRole as clap sees it: its FromStr and its
/// Display.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum SubagentMcpRole {
    Parent,
    MessageOnly,
    FetchOnly,
    Child,
}

impl SubagentMcpRole {
    fn id(self) -> &'static str {
        match self {
            Self::Parent => "parent",
            Self::MessageOnly => "message_only",
            Self::FetchOnly => "fetch_only",
            Self::Child => "child",
        }
    }
}

impl std::fmt::Display for SubagentMcpRole {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str(self.id())
    }
}

impl std::str::FromStr for SubagentMcpRole {
    type Err = String;

    /// The real error is `anyhow::bail!("unknown sub-agent MCP role
    /// {other:?}")`, whose Display is this text.
    fn from_str(value: &str) -> Result<Self, String> {
        match value {
            "parent" => Ok(Self::Parent),
            "fixed_parent" => Ok(Self::Parent),
            "message_only" => Ok(Self::MessageOnly),
            "fetch_only" => Ok(Self::FetchOnly),
            "child" => Ok(Self::Child),
            other => Err(format!("unknown sub-agent MCP role {other:?}")),
        }
    }
}

impl serde::Serialize for SubagentMcpRole {
    fn serialize<S: serde::Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        serializer.collect_str(self)
    }
}

/// The derive-based ops: parse with `P`, or render its Command.
fn derived<P: clap::Parser + serde::Serialize>(request: &Value, op: &str) -> R<Value> {
    let parse = |argv: &[String]| match P::try_parse_from(argv) {
        Ok(parsed) => serde_json::to_value(&parsed)
            .map(|value| json!({ "ok": value }))
            .map_err(|error| error.to_string()),
        Err(error) => Ok(json!({ "err": dump_error(&error) })),
    };
    if op.ends_with("_render") {
        return render(P::command(), request);
    }
    if op.ends_with("_runs") {
        let mut results = Vec::new();
        for argv in argvs(request)? {
            results.push(parse(&argv)?);
        }
        return Ok(json!({ "results": results }));
    }
    parse(&strings(request.get("argv"))?)
}
