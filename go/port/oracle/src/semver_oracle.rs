//! Oracle module `semver`. Requests are objects with an `"op"` member; see the
//! Go tests that use this module (`go/crates/semver/oracle_test.go`) for the
//! operations it serves.
//!
//! - `parse`: `Version::parse(text)`, with every field, the Display text and
//!   the Debug text of the parts, or the error's Display and Debug text.
//! - `prerelease`, `build`: `Prerelease::new(text)`, `BuildMetadata::new(text)`.
//! - `cmp`: two versions compared every way the crate offers.
//! - `cmp_pre`, `cmp_build`: two identifiers compared.
//! - `sort`: versions sorted by `Ord` and, stably, by `cmp_precedence`.
//! - `debug`: the `{:?}` and `{:#?}` text of a version, alone and as a field
//!   of a struct whose Debug is derived (as Mjolnir's `BuildIdentity` holds
//!   one).
//! - `format`: a value through one of a fixed list of format specs. `what`
//!   says which value: `version` (the default), `pre`, `build`, or `error`
//!   (the error `Version::parse(text)` returns).

use std::cmp::Ordering;
use std::fmt::{Debug, Display};

use semver::{BuildMetadata, Prerelease, Version};
use serde_json::{Value, json};

pub fn handle(request: &Value) -> Value {
    match crate::text(request, "op") {
        "parse" => parse(request),
        "prerelease" => prerelease(request),
        "build" => build(request),
        "cmp" => cmp(request),
        "cmp_pre" => cmp_pre(request),
        "cmp_build" => cmp_build(request),
        "sort" => sort(request),
        "debug" => debug(request),
        "format" => format(request),
        _ => crate::unknown(request),
    }
}

fn ordering(order: Ordering) -> i32 {
    match order {
        Ordering::Less => -1,
        Ordering::Equal => 0,
        Ordering::Greater => 1,
    }
}

fn error(error: &semver::Error) -> Value {
    json!({"err": error.to_string(), "err_debug": format!("{:?}", error)})
}

fn parse(request: &Value) -> Value {
    let text = crate::text(request, "text");
    match Version::parse(text) {
        Ok(version) => {
            let again: Version = text.parse().expect("FromStr agrees with parse");
            assert_eq!(again, version);
            let rebuilt = Version {
                major: version.major,
                minor: version.minor,
                patch: version.patch,
                pre: Prerelease::new(version.pre.as_str()).expect("a parsed pre-release parses"),
                build: BuildMetadata::new(version.build.as_str()).expect("parsed build metadata parses"),
            };
            json!({"ok": {
                "major": version.major,
                "minor": version.minor,
                "patch": version.patch,
                "pre": version.pre.as_str(),
                "build": version.build.as_str(),
                "pre_empty": version.pre.is_empty(),
                "build_empty": version.build.is_empty(),
                "pre_len": version.pre.len(),
                "build_len": version.build.len(),
                "display": version.to_string(),
                "pre_display": version.pre.to_string(),
                "build_display": version.build.to_string(),
                "pre_debug": format!("{:?}", version.pre),
                "build_debug": format!("{:?}", version.build),
                "rebuilt_eq": rebuilt == version,
                "new_eq": Version::new(version.major, version.minor, version.patch) == version,
            }})
        }
        Err(e) => error(&e),
    }
}

fn prerelease(request: &Value) -> Value {
    match Prerelease::new(crate::text(request, "text")) {
        Ok(pre) => json!({"ok": {
            "as_str": pre.as_str(),
            "is_empty": pre.is_empty(),
            "len": pre.len(),
            "display": pre.to_string(),
            "debug": format!("{:?}", pre),
            "is_default": pre == Prerelease::default(),
            "is_const_empty": pre == Prerelease::EMPTY,
        }}),
        Err(e) => error(&e),
    }
}

fn build(request: &Value) -> Value {
    match BuildMetadata::new(crate::text(request, "text")) {
        Ok(build) => json!({"ok": {
            "as_str": build.as_str(),
            "is_empty": build.is_empty(),
            "len": build.len(),
            "display": build.to_string(),
            "debug": format!("{:?}", build),
            "is_default": build == BuildMetadata::default(),
            "is_const_empty": build == BuildMetadata::EMPTY,
        }}),
        Err(e) => error(&e),
    }
}

fn cmp(request: &Value) -> Value {
    let (Ok(a), Ok(b)) = (Version::parse(crate::text(request, "a")), Version::parse(crate::text(request, "b"))) else {
        return json!({"invalid": true});
    };
    json!({
        "cmp": ordering(a.cmp(&b)),
        "partial_cmp": a.partial_cmp(&b).map(ordering),
        "precedence": ordering(a.cmp_precedence(&b)),
        "eq": a == b,
        "lt": a < b,
        "le": a <= b,
        "gt": a > b,
        "ge": a >= b,
        "pre": ordering(a.pre.cmp(&b.pre)),
        "build": ordering(a.build.cmp(&b.build)),
        "pre_eq": a.pre == b.pre,
        "build_eq": a.build == b.build,
        "max": std::cmp::max(a.clone(), b.clone()).to_string(),
        "min": std::cmp::min(a.clone(), b.clone()).to_string(),
    })
}

fn cmp_pre(request: &Value) -> Value {
    let (Ok(a), Ok(b)) = (Prerelease::new(crate::text(request, "a")), Prerelease::new(crate::text(request, "b"))) else {
        return json!({"invalid": true});
    };
    json!({"cmp": ordering(a.cmp(&b)), "partial_cmp": a.partial_cmp(&b).map(ordering), "eq": a == b, "lt": a < b, "self": ordering(a.cmp(&a.clone()))})
}

fn cmp_build(request: &Value) -> Value {
    let (Ok(a), Ok(b)) = (BuildMetadata::new(crate::text(request, "a")), BuildMetadata::new(crate::text(request, "b"))) else {
        return json!({"invalid": true});
    };
    json!({"cmp": ordering(a.cmp(&b)), "partial_cmp": a.partial_cmp(&b).map(ordering), "eq": a == b, "lt": a < b, "self": ordering(a.cmp(&a.clone()))})
}

fn versions(request: &Value) -> Option<Vec<Version>> {
    request
        .get("versions")
        .and_then(Value::as_array)?
        .iter()
        .map(|text| Version::parse(text.as_str().unwrap_or_default()).ok())
        .collect()
}

fn sort(request: &Value) -> Value {
    let Some(versions) = versions(request) else {
        return json!({"invalid": true});
    };
    let mut by_precedence = versions.clone();
    // A stable sort: versions that differ only in build metadata keep their order.
    by_precedence.sort_by(Version::cmp_precedence);
    let mut total = versions;
    total.sort();
    json!({
        "by_precedence": by_precedence.iter().map(Version::to_string).collect::<Vec<_>>(),
        "total": total.iter().map(Version::to_string).collect::<Vec<_>>(),
    })
}

/// A struct whose Debug is derived, holding a version the way Mjolnir's
/// `BuildIdentity` and `UpdateInfo` do.
#[derive(Debug)]
#[allow(dead_code)]
struct Wrapper<'a> {
    version: &'a Version,
    revision: Option<&'a str>,
}

fn debug(request: &Value) -> Value {
    match Version::parse(crate::text(request, "text")) {
        Ok(version) => {
            let wrapper = Wrapper {
                version: &version,
                revision: Some("abc"),
            };
            json!({
                "debug": format!("{:?}", version),
                "pretty": format!("{:#?}", version),
                "option": format!("{:?}", Some(&version)),
                "list": format!("{:?}", [&version, &version]),
                "nested": format!("{:?}", wrapper),
                "nested_pretty": format!("{:#?}", wrapper),
            })
        }
        Err(e) => error(&e),
    }
}

/// The format specs the `format` operation knows. A spec reaches the value's
/// Display or Debug impl with its width, fill, alignment, precision and
/// flags.
fn format_with<T: Display + Debug>(spec: &str, value: &T) -> Option<String> {
    Some(match spec {
        "{}" => format!("{}", value),
        "{:20}" => format!("{:20}", value),
        "{:<20}" => format!("{:<20}", value),
        "{:>20}" => format!("{:>20}", value),
        "{:^20}" => format!("{:^20}", value),
        "{:*^20}" => format!("{:*^20}", value),
        "{:*^21}" => format!("{:*^21}", value),
        "{:-<9}" => format!("{:-<9}", value),
        "{:0>12}" => format!("{:0>12}", value),
        "{:012}" => format!("{:012}", value),
        "{:5}" => format!("{:5}", value),
        "{:.3}" => format!("{:.3}", value),
        "{:10.3}" => format!("{:10.3}", value),
        "{:é^11}" => format!("{:é^11}", value),
        "{:🦀<12}" => format!("{:🦀<12}", value),
        "{:>1}" => format!("{:>1}", value),
        "{:^9}" => format!("{:^9}", value),
        "{:+}" => format!("{:+}", value),
        "{:#}" => format!("{:#}", value),
        "{:+>14}" => format!("{:+>14}", value),
        "{:#?}" => format!("{:#?}", value),
        "{:?}" => format!("{:?}", value),
        "{:8?}" => format!("{:8?}", value),
        "{:>40?}" => format!("{:>40?}", value),
        _ => return None,
    })
}

fn format(request: &Value) -> Value {
    let text = crate::text(request, "text");
    let spec = crate::text(request, "spec");
    let formatted = match crate::text(request, "what") {
        "" | "version" => match Version::parse(text) {
            Ok(version) => format_with(spec, &version),
            Err(e) => return error(&e),
        },
        "pre" => match Prerelease::new(text) {
            Ok(pre) => format_with(spec, &pre),
            Err(e) => return error(&e),
        },
        "build" => match BuildMetadata::new(text) {
            Ok(build) => format_with(spec, &build),
            Err(e) => return error(&e),
        },
        "error" => match Version::parse(text) {
            Ok(_) => return json!({"invalid": true}),
            Err(e) => format_with(spec, &e),
        },
        _ => None,
    };
    match formatted {
        Some(text) => json!({"text": text}),
        None => crate::unknown(request),
    }
}
