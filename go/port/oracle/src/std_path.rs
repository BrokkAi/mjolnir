//! Oracle module `std_path`: `std::path`, `std::ffi::OsStr` and `std::env`,
//! for the Go packages `go/rs/rspath` and `go/rs/rsenv`.
//!
//! Requests are objects with an `"op"` member. A path or an `OsStr` travels as
//! the hexadecimal form of its encoded bytes (`OsStr::as_encoded_bytes`), so
//! that bytes which are not Unicode survive JSON: any bytes on Unix, WTF-8 on
//! Windows. The Go tests that use this module only send WTF-8 to a Windows
//! build. Text that Rust guarantees to be UTF-8 (lossy conversions, `Debug`
//! and `Display` output) travels as a JSON string.
//!
//! Operations:
//!
//! - `inspect {p, pretty}`: everything `Path` says about one path; with
//!   `pretty`, also the `{:#?}` forms of its iterators.
//! - `pair {p, q}`: everything that takes two paths, or a path and a name.
//! - `walk {p, steps}`: `Components` driven from both ends, one letter a step.
//! - `cmp_components {p, q, fp, bp, fq, bq}`: `Components` compared after
//!   taking some from the front and the back of each.
//! - `os_str {p, q}`: `OsStr` and `OsString` methods.
//! - `debug_classes {}`: how the `Debug` form writes every `char`.
//! - `absolute {p, cwd}`: `std::path::absolute` from a given directory.
//! - `absolute_deleted_cwd {p}`: the same from a directory that is gone.
//! - `split_paths {s}`, `join_paths {parts}`: `std::env`.
//! - `var_error {s}`: `VarError` texts.
//! - `env_set_get {k, v}`, `env_get {k}`, `env_remove {k}`: the environment.
//! - `chdir {p}`: `set_current_dir` then `current_dir`.
//! - `temp_dir {tmpdir}`, `home_dir {home}`, `consts {}`, `current_exe {}`.

use std::cmp::Ordering;
use std::ffi::{OsStr, OsString};
use std::hash::{DefaultHasher, Hash, Hasher};
use std::panic::{AssertUnwindSafe, catch_unwind};
use std::path::{Component, Components, Path, PathBuf, Prefix};
use std::sync::Once;

use serde_json::{Value, json};

static QUIET: Once = Once::new();

pub fn handle(request: &Value) -> Value {
    // Panics are answers here (an extension holding a separator, a variable
    // name the system refuses), and each would otherwise print a report.
    QUIET.call_once(|| std::panic::set_hook(Box::new(|_| {})));

    match crate::text(request, "op") {
        "inspect" => inspect(
            &path(request, "p"),
            request.get("pretty").and_then(Value::as_bool).unwrap_or(false),
        ),
        "pair" => pair(&path(request, "p"), &os(request, "q")),
        "walk" => walk(&path(request, "p"), crate::text(request, "steps")),
        "cmp_components" => cmp_components(request),
        "os_str" => os_str(&os(request, "p"), &os(request, "q")),
        "debug_classes" => debug_classes(),
        "absolute" => absolute(request),
        "absolute_deleted_cwd" => absolute_deleted_cwd(request),
        "split_paths" => Value::Array(
            std::env::split_paths(&os(request, "s"))
                .map(|part| hex(part.as_os_str()))
                .collect(),
        ),
        "join_paths" => join_paths(request),
        "var_error" => var_error(request),
        "env_set_get" => env_set_get(&os(request, "k"), &os(request, "v")),
        "env_get" => env_get(&os(request, "k")),
        "env_remove" => {
            let key = os(request, "k");
            match catch(|| unsafe { std::env::remove_var(&key) }) {
                Ok(()) => json!({ "ok": true }),
                Err(message) => json!({ "panic": message }),
            }
        }
        "chdir" => chdir(&path(request, "p")),
        "temp_dir" => {
            match optional_os(request, "tmpdir") {
                Some(value) => unsafe { std::env::set_var("TMPDIR", value) },
                None => unsafe { std::env::remove_var("TMPDIR") },
            }
            hex(std::env::temp_dir().as_os_str())
        }
        "home_dir" => {
            let variable = if cfg!(windows) { "USERPROFILE" } else { "HOME" };
            match optional_os(request, "home") {
                Some(value) => unsafe { std::env::set_var(variable, value) },
                None => unsafe { std::env::remove_var(variable) },
            }
            optional_hex(std::env::home_dir().as_deref().map(Path::as_os_str))
        }
        "consts" => {
            let separators = ['/', '\\', ':', '.', 'a', '\u{2215}']
                .iter()
                .map(|c| std::path::is_separator(*c))
                .collect::<Vec<_>>();
            json!({
            "ARCH": std::env::consts::ARCH,
            "FAMILY": std::env::consts::FAMILY,
            "OS": std::env::consts::OS,
            "DLL_PREFIX": std::env::consts::DLL_PREFIX,
            "DLL_SUFFIX": std::env::consts::DLL_SUFFIX,
            "DLL_EXTENSION": std::env::consts::DLL_EXTENSION,
            "EXE_SUFFIX": std::env::consts::EXE_SUFFIX,
            "EXE_EXTENSION": std::env::consts::EXE_EXTENSION,
            "MAIN_SEPARATOR": std::path::MAIN_SEPARATOR.to_string(),
            "MAIN_SEPARATOR_STR": std::path::MAIN_SEPARATOR_STR,
            "is_separator": separators,
            })
        }
        "current_exe" => match std::env::current_exe() {
            Ok(exe) => json!({
                "is_absolute": exe.is_absolute(),
                "file_name": optional_hex(exe.file_name()),
                "args0_file_name": optional_hex(
                    std::env::args_os().next().as_deref().map(Path::new).and_then(Path::file_name)
                ),
                "args": std::env::args_os().count(),
            }),
            Err(error) => io_error(&error),
        },
        _ => crate::unknown(request),
    }
}

// --- encoding ------------------------------------------------------------

fn hex_bytes(bytes: &[u8]) -> String {
    const DIGITS: &[u8; 16] = b"0123456789abcdef";
    let mut out = String::with_capacity(bytes.len() * 2);
    for byte in bytes {
        out.push(DIGITS[(byte >> 4) as usize] as char);
        out.push(DIGITS[(byte & 15) as usize] as char);
    }
    out
}

fn hex(text: &OsStr) -> Value {
    Value::String(hex_bytes(text.as_encoded_bytes()))
}

fn optional_hex(text: Option<&OsStr>) -> Value {
    text.map(hex).unwrap_or(Value::Null)
}

fn unhex(text: &str) -> Vec<u8> {
    let digit = |byte: u8| match byte {
        b'0'..=b'9' => byte - b'0',
        b'a'..=b'f' => byte - b'a' + 10,
        _ => panic!("oracle: bad hexadecimal digit"),
    };
    text.as_bytes()
        .chunks(2)
        .map(|pair| digit(pair[0]) << 4 | digit(pair[1]))
        .collect()
}

fn os_from(text: &str) -> OsString {
    // SAFETY: the Go tests send the encoded bytes of an OsStr: anything on
    // Unix, and only WTF-8 to a Windows build.
    unsafe { OsString::from_encoded_bytes_unchecked(unhex(text)) }
}

fn os(request: &Value, key: &str) -> OsString {
    os_from(crate::text(request, key))
}

fn optional_os(request: &Value, key: &str) -> Option<OsString> {
    request.get(key).and_then(Value::as_str).map(os_from)
}

fn path(request: &Value, key: &str) -> PathBuf {
    PathBuf::from(os(request, key))
}

fn ordering(order: Ordering) -> i32 {
    match order {
        Ordering::Less => -1,
        Ordering::Equal => 0,
        Ordering::Greater => 1,
    }
}

fn catch<T>(run: impl FnOnce() -> T) -> Result<T, String> {
    catch_unwind(AssertUnwindSafe(run)).map_err(|payload| {
        payload
            .downcast_ref::<String>()
            .cloned()
            .or_else(|| {
                payload
                    .downcast_ref::<&str>()
                    .map(|text| (*text).to_owned())
            })
            .unwrap_or_else(|| "non-string panic payload".to_owned())
    })
}

fn io_error(error: &std::io::Error) -> Value {
    json!({
        "err": error.to_string(),
        "kind": format!("{:?}", error.kind()),
        "os": error.raw_os_error(),
    })
}

fn prefix(prefix: Prefix<'_>) -> Value {
    let (kind, first, second, disk) = match prefix {
        Prefix::Verbatim(a) => ("Verbatim", Some(a), None, None),
        Prefix::VerbatimUNC(a, b) => ("VerbatimUNC", Some(a), Some(b), None),
        Prefix::VerbatimDisk(disk) => ("VerbatimDisk", None, None, Some(disk)),
        Prefix::DeviceNS(a) => ("DeviceNS", Some(a), None, None),
        Prefix::UNC(a, b) => ("UNC", Some(a), Some(b), None),
        Prefix::Disk(disk) => ("Disk", None, None, Some(disk)),
    };
    json!({
        "k": kind,
        "a": optional_hex(first),
        "b": optional_hex(second),
        "d": disk,
        "verbatim": prefix.is_verbatim(),
        "dbg": format!("{prefix:?}"),
    })
}

fn component(component: Component<'_>) -> Value {
    let (kind, value, parsed) = match component {
        Component::Prefix(p) => ("Prefix", None, Some(prefix(p.kind()))),
        Component::RootDir => ("RootDir", None, None),
        Component::CurDir => ("CurDir", None, None),
        Component::ParentDir => ("ParentDir", None, None),
        Component::Normal(name) => ("Normal", Some(name), None),
    };
    json!({
        "t": kind,
        "v": optional_hex(value),
        "prefix": parsed,
        "as": hex(component.as_os_str()),
        "dbg": format!("{component:?}"),
    })
}

fn optional_component(component: Option<Component<'_>>) -> Value {
    component.map(self::component).unwrap_or(Value::Null)
}

fn hash_of(path: &Path) -> u64 {
    let mut hasher = DefaultHasher::new();
    path.hash(&mut hasher);
    hasher.finish()
}

// --- std::path -----------------------------------------------------------

fn inspect(path: &Path, pretty: bool) -> Value {
    let mut popped = path.to_path_buf();
    let did_pop = popped.pop();
    let mut reply = json!({
        "components": path.components().map(component).collect::<Vec<_>>(),
        "components_rev": path
            .components()
            .rev()
            .map(|component| format!("{component:?}"))
            .collect::<Vec<_>>(),
        "iter": path.iter().map(hex).collect::<Vec<_>>(),
        "iter_rev": path.iter().rev().map(hex).collect::<Vec<_>>(),
        "ancestors": path.ancestors().map(|p| hex(p.as_os_str())).collect::<Vec<_>>(),
        "count": path.components().count(),
        "parent": optional_hex(path.parent().map(Path::as_os_str)),
        "file_name": optional_hex(path.file_name()),
        "file_stem": optional_hex(path.file_stem()),
        "file_prefix": optional_hex(path.file_prefix()),
        "extension": optional_hex(path.extension()),
        "is_absolute": path.is_absolute(),
        "is_relative": path.is_relative(),
        "has_root": path.has_root(),
        "is_empty": path.as_os_str().is_empty(),
        "to_str": path.to_str(),
        "lossy": path.to_string_lossy(),
        "display": path.display().to_string(),
        "debug": format!("{path:?}"),
        "into_string": path.to_path_buf().into_os_string().into_string().ok(),
        "components_debug": format!("{:?}", path.components()),
        "iter_debug": format!("{:?}", path.iter()),
        "pop": [hex(popped.as_os_str()), Value::Bool(did_pop)],
        "collected": hex(path.components().collect::<PathBuf>().as_os_str()),
        "self_eq": path == path,
    });
    if pretty {
        reply["components_pretty"] = json!(format!("{:#?}", path.components()));
        reply["components_list_pretty"] =
            json!(format!("{:#?}", path.components().collect::<Vec<_>>()));
        reply["iter_pretty"] = json!(format!("{:#?}", path.iter()));
    }
    reply
}

fn pair(path: &Path, other: &OsStr) -> Value {
    let other_path = Path::new(other);

    let strip = path.strip_prefix(other_path);
    let (strip_ok, strip_err) = match &strip {
        Ok(rest) => (hex(rest.as_os_str()), Value::Null),
        Err(error) => (Value::Null, Value::String(error.to_string())),
    };

    let mut set_file_name = path.to_path_buf();
    set_file_name.set_file_name(other);

    let set_extension = catch(|| {
        let mut buf = path.to_path_buf();
        let changed = buf.set_extension(other);
        json!([hex(buf.as_os_str()), changed])
    });
    let add_extension = catch(|| {
        let mut buf = path.to_path_buf();
        let changed = buf.add_extension(other);
        json!([hex(buf.as_os_str()), changed])
    });
    let with_extension = catch(|| hex(path.with_extension(other).as_os_str()));
    let with_added_extension = catch(|| hex(path.with_added_extension(other).as_os_str()));
    let or_panic = |result: Result<Value, String>| match result {
        Ok(value) => value,
        Err(message) => json!({ "panic": message }),
    };

    json!({
        "join": hex(path.join(other_path).as_os_str()),
        "starts_with": path.starts_with(other_path),
        "ends_with": path.ends_with(other_path),
        "strip_prefix": strip_ok,
        "strip_prefix_err": strip_err,
        "eq": path == other_path,
        "cmp": ordering(path.cmp(other_path)),
        // Equal paths hash alike. The converse does not hold (`/a` and `a`
        // hash alike), so the hashes themselves are not compared.
        "eq_implies_hash_eq": path != other_path || hash_of(path) == hash_of(other_path),
        "components_eq": path.components() == other_path.components(),
        "components_cmp": ordering(path.components().cmp(other_path.components())),
        "with_file_name": hex(path.with_file_name(other).as_os_str()),
        "set_file_name": hex(set_file_name.as_os_str()),
        "set_extension": or_panic(set_extension),
        "add_extension": or_panic(add_extension),
        "with_extension": or_panic(with_extension),
        "with_added_extension": or_panic(with_added_extension),
    })
}

fn walk(path: &Path, steps: &str) -> Value {
    let mut components = path.components();
    let mut out = Vec::new();
    for step in steps.chars() {
        out.push(match step {
            'F' => optional_component(components.next()),
            'B' => optional_component(components.next_back()),
            'P' => hex(components.as_path().as_os_str()),
            'D' => Value::String(format!("{components:?}")),
            _ => panic!("oracle: unknown walk step"),
        });
    }
    Value::Array(out)
}

fn advanced<'a>(path: &'a Path, front: u64, back: u64) -> Components<'a> {
    let mut components = path.components();
    for _ in 0..front {
        components.next();
    }
    for _ in 0..back {
        components.next_back();
    }
    components
}

fn cmp_components(request: &Value) -> Value {
    let count = |key: &str| request.get(key).and_then(Value::as_u64).unwrap_or(0);
    let p = path(request, "p");
    let q = path(request, "q");
    let left = advanced(&p, count("fp"), count("bp"));
    let right = advanced(&q, count("fq"), count("bq"));
    json!({
        "eq": left == right,
        "cmp": ordering(left.clone().cmp(right.clone())),
        "left": hex(left.as_path().as_os_str()),
        "right": hex(right.as_path().as_os_str()),
    })
}

fn os_str(first: &OsStr, second: &OsStr) -> Value {
    let mut pushed = first.to_os_string();
    pushed.push(second);
    json!({
        "to_str": first.to_str(),
        "lossy": first.to_string_lossy(),
        "display": first.display().to_string(),
        "debug": format!("{first:?}"),
        "into_string": first.to_os_string().into_string().ok(),
        "into_string_err": first.to_os_string().into_string().err().map(|error| format!("{error:?}")),
        "len": first.len(),
        "is_empty": first.is_empty(),
        "is_ascii": first.is_ascii(),
        "lower": hex(&first.to_ascii_lowercase()),
        "upper": hex(&first.to_ascii_uppercase()),
        "push": hex(&pushed),
        "eq": first == second,
        "cmp": ordering(first.cmp(second)),
        "eq_ignore_ascii_case": first.eq_ignore_ascii_case(second),
        "join": hex(&[first, second, first].join(second)),
        "wide": wide(first),
    })
}

#[cfg(windows)]
fn wide(text: &OsStr) -> Value {
    use std::os::windows::ffi::{OsStrExt, OsStringExt};
    let units = text.encode_wide().collect::<Vec<u16>>();
    json!({ "units": units, "back": hex(&OsString::from_wide(&units)) })
}

#[cfg(not(windows))]
fn wide(_text: &OsStr) -> Value {
    Value::Null
}

/// How `{:?}` of an `OsStr` writes each `char`: as itself (0), as `\u{..}` (1)
/// or with a backslash escape (2, listed in `specials`). Runs of code points
/// of one class are given as `[first, last, class]`.
fn debug_classes() -> Value {
    let mut ranges: Vec<[u32; 3]> = Vec::new();
    let mut specials = serde_json::Map::new();
    for code in 0..=0x10FFFF_u32 {
        let Some(c) = char::from_u32(code) else {
            continue;
        };
        let mut buffer = [0_u8; 4];
        let text: &str = c.encode_utf8(&mut buffer);
        let shown = format!("{:?}", OsStr::new(text));
        let class = if shown == format!("\"{c}\"") {
            0
        } else if shown == format!("\"\\u{{{code:x}}}\"") {
            1
        } else {
            specials.insert(code.to_string(), Value::String(shown));
            2
        };
        match ranges.last_mut() {
            Some(last) if last[2] == class && last[1] + 1 == code => last[1] = code,
            _ => ranges.push([code, code, class]),
        }
    }
    json!({ "ranges": ranges, "specials": specials })
}

/// `std::path::absolute` when the current directory no longer exists. The
/// process is left in the root directory.
fn absolute_deleted_cwd(request: &Value) -> Value {
    let directory = std::env::temp_dir().join(format!("mj-oracle-gone-{}", std::process::id()));
    if let Err(error) = std::fs::create_dir(&directory)
        .and_then(|()| std::env::set_current_dir(&directory))
        .and_then(|()| std::fs::remove_dir(&directory))
    {
        return json!({ "oracle_error": format!("cannot prepare a deleted directory: {error}") });
    }
    let reply = match std::path::absolute(path(request, "p")) {
        Ok(absolute) => json!({ "ok": hex(absolute.as_os_str()) }),
        Err(error) => io_error(&error),
    };
    let current = match std::env::current_dir() {
        Ok(current) => json!({ "ok": hex(current.as_os_str()) }),
        Err(error) => io_error(&error),
    };
    let root = if cfg!(windows) { "C:\\" } else { "/" };
    if let Err(error) = std::env::set_current_dir(root) {
        return json!({ "oracle_error": format!("cannot leave the deleted directory: {error}") });
    }
    json!({ "absolute": reply, "current_dir": current })
}

fn absolute(request: &Value) -> Value {
    if let Some(directory) = optional_os(request, "cwd") {
        if let Err(error) = std::env::set_current_dir(&directory) {
            return json!({ "oracle_error": format!("cannot enter the directory: {error}") });
        }
    }
    match std::path::absolute(path(request, "p")) {
        Ok(absolute) => json!({ "ok": hex(absolute.as_os_str()) }),
        Err(error) => io_error(&error),
    }
}

// --- std::env ------------------------------------------------------------

fn join_paths(request: &Value) -> Value {
    let parts = request
        .get("parts")
        .and_then(Value::as_array)
        .map(|parts| {
            parts
                .iter()
                .filter_map(Value::as_str)
                .map(os_from)
                .collect::<Vec<_>>()
        })
        .unwrap_or_default();
    match std::env::join_paths(parts) {
        Ok(joined) => json!({ "ok": hex(&joined) }),
        Err(error) => json!({ "err": error.to_string(), "debug": format!("{error:?}") }),
    }
}

fn var_result(result: Result<String, std::env::VarError>) -> Value {
    match result {
        Ok(value) => json!({ "ok": value }),
        Err(error) => json!({ "err": error.to_string(), "debug": format!("{error:?}") }),
    }
}

fn var_error(request: &Value) -> Value {
    let error = match optional_os(request, "s") {
        Some(value) => std::env::VarError::NotUnicode(value),
        None => std::env::VarError::NotPresent,
    };
    json!({ "display": error.to_string(), "debug": format!("{error:?}") })
}

fn env_get(key: &OsStr) -> Value {
    json!({
        "var_os": optional_hex(std::env::var_os(key).as_deref()),
        "var": var_result(std::env::var(key)),
    })
}

fn env_set_get(key: &OsStr, value: &OsStr) -> Value {
    if let Err(message) = catch(|| unsafe { std::env::set_var(key, value) }) {
        return json!({ "set_panic": message });
    }
    let after_set = env_get(key);
    let listed = std::env::vars_os().any(|(name, held)| name == key && held == value);
    let removed = match catch(|| unsafe { std::env::remove_var(key) }) {
        Ok(()) => env_get(key),
        Err(message) => json!({ "remove_panic": message }),
    };
    json!({ "set": after_set, "listed": listed, "removed": removed })
}

fn chdir(directory: &Path) -> Value {
    match std::env::set_current_dir(directory) {
        Ok(()) => match std::env::current_dir() {
            Ok(current) => json!({ "ok": hex(current.as_os_str()) }),
            Err(error) => io_error(&error),
        },
        Err(error) => io_error(&error),
    }
}
