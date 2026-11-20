//! Oracle module `url`. Requests are objects with an `"op"` member; see the
//! Go tests in go/crates/url that use this module for the operations it
//! serves.

use std::cell::RefCell;
use std::path::{Path, PathBuf};

use serde_json::{Value, json};
use url::{Host, Origin, ParseError, Position, Url, form_urlencoded, quirks};

fn hex(bytes: &[u8]) -> String {
    bytes.iter().map(|byte| format!("{byte:02x}")).collect()
}

fn unhex(text: &str) -> Vec<u8> {
    (0..text.len() / 2)
        .map(|index| u8::from_str_radix(&text[2 * index..2 * index + 2], 16).unwrap())
        .collect()
}

fn host_value<S: AsRef<str>>(host: &Host<S>) -> Value {
    match host {
        Host::Domain(domain) => json!({ "Domain": domain.as_ref() }),
        Host::Ipv4(address) => json!({ "Ipv4": address.octets().to_vec() }),
        Host::Ipv6(address) => json!({ "Ipv6": address.octets().to_vec() }),
    }
}

fn error_value(error: ParseError) -> Value {
    json!({ "err": format!("{error:?}"), "display": error.to_string() })
}

const POSITIONS: [Position; 16] = [
    Position::BeforeScheme,
    Position::AfterScheme,
    Position::BeforeUsername,
    Position::AfterUsername,
    Position::BeforePassword,
    Position::AfterPassword,
    Position::BeforeHost,
    Position::AfterHost,
    Position::BeforePort,
    Position::AfterPort,
    Position::BeforePath,
    Position::AfterPath,
    Position::BeforeQuery,
    Position::AfterQuery,
    Position::BeforeFragment,
    Position::AfterFragment,
];

/// Everything a `Url` says about itself, without anything that depends on
/// debug assertions.
fn dump(url: &Url) -> Value {
    let origin = url.origin();
    let origin_value = match &origin {
        Origin::Opaque(_) => json!({ "opaque": true }),
        Origin::Tuple(scheme, host, port) => {
            json!({ "scheme": scheme, "host": host_value(host), "port": port })
        }
    };
    // `&url[..position]` for every position. The two password positions are
    // left out when the URL has a username and no password: in that state
    // the crate's debug assertions fail, and release and debug builds differ.
    let has_password = url.password().is_some();
    let has_username = !url.username().is_empty();
    let positions: Vec<Value> = POSITIONS
        .iter()
        .map(|position| {
            if matches!(position, Position::BeforePassword | Position::AfterPassword)
                && has_username
                && !has_password
            {
                Value::Null
            } else {
                json!(url[..*position].len())
            }
        })
        .collect();
    json!({
        "href": url.as_str(),
        "to_string": url.to_string(),
        "into_string": String::from(url.clone()),
        "scheme": url.scheme(),
        "is_special": url.is_special(),
        "has_authority": url.has_authority(),
        "authority": url.authority(),
        "cannot_be_a_base": url.cannot_be_a_base(),
        "username": url.username(),
        "password": url.password(),
        "has_host": url.has_host(),
        "host_str": url.host_str(),
        "host": url.host().as_ref().map(host_value),
        "domain": url.domain(),
        "port": url.port(),
        "port_or_known_default": url.port_or_known_default(),
        "path": url.path(),
        "path_segments": url.path_segments().map(|segments| segments.collect::<Vec<_>>()),
        "query": url.query(),
        "query_pairs": url
            .query_pairs()
            .map(|(name, value)| json!([name, value]))
            .collect::<Vec<_>>(),
        "fragment": url.fragment(),
        "origin": origin_value,
        "origin_is_tuple": origin.is_tuple(),
        "origin_ascii": origin.ascii_serialization(),
        "origin_unicode": origin.unicode_serialization(),
        "positions": positions,
        "full": &url[..],
        "quirks": {
            "href": quirks::href(url),
            "origin": quirks::origin(url),
            "protocol": quirks::protocol(url),
            "username": quirks::username(url),
            "password": quirks::password(url),
            "host": quirks::host(url),
            "hostname": quirks::hostname(url),
            "port": quirks::port(url),
            "pathname": quirks::pathname(url),
            "search": quirks::search(url),
            "hash": quirks::hash(url),
        },
        "check_invariants": match url.check_invariants() {
            Ok(()) => Value::Null,
            Err(message) => json!(message),
        },
    })
}

/// The components of a `Url`, for the cases that are too many to record a
/// whole `dump` of.
fn brief(url: &Url) -> Value {
    json!([
        url.as_str(),
        url.scheme(),
        url.username(),
        url.password(),
        url.host_str(),
        url.host().as_ref().map(host_value),
        url.port(),
        url.path(),
        url.query(),
        url.fragment(),
    ])
}

fn parse_result(result: Result<Url, ParseError>, request: &Value) -> Value {
    match result {
        Ok(url) if request["brief"].as_bool().unwrap_or(false) => json!({ "ok": brief(&url) }),
        Ok(url) => json!({ "ok": dump(&url) }),
        Err(error) => error_value(error),
    }
}

fn option_text(request: &Value, key: &str) -> Option<String> {
    request.get(key).and_then(Value::as_str).map(str::to_owned)
}

fn unit_result(result: Result<(), ()>) -> Value {
    match result {
        Ok(()) => json!("ok"),
        Err(()) => json!("err"),
    }
}

/// Applies one setter. `value` is absent for the `None` of a setter that
/// takes an option.
fn apply_setter(url: &mut Url, step: &Value) -> Value {
    let name = crate::text(step, "set");
    let value = option_text(step, "value");
    let text = value.clone().unwrap_or_default();
    match name {
        "fragment" => {
            url.set_fragment(value.as_deref());
            json!("ok")
        }
        "query" => {
            url.set_query(value.as_deref());
            json!("ok")
        }
        "path" => {
            url.set_path(&text);
            json!("ok")
        }
        "port" => {
            let port = step.get("port").and_then(Value::as_u64).map(|port| port as u16);
            unit_result(url.set_port(port))
        }
        "host" => match url.set_host(value.as_deref()) {
            Ok(()) => json!("ok"),
            Err(error) => error_value(error),
        },
        "ip_host" => unit_result(url.set_ip_host(text.parse().unwrap())),
        "password" => unit_result(url.set_password(value.as_deref())),
        "username" => unit_result(url.set_username(&text)),
        "scheme" => unit_result(url.set_scheme(&text)),
        "quirks_href" => match quirks::set_href(url, &text) {
            Ok(()) => json!("ok"),
            Err(error) => error_value(error),
        },
        "quirks_protocol" => unit_result(quirks::set_protocol(url, &text)),
        "quirks_username" => unit_result(quirks::set_username(url, &text)),
        "quirks_password" => unit_result(quirks::set_password(url, &text)),
        "quirks_host" => unit_result(quirks::set_host(url, &text)),
        "quirks_hostname" => unit_result(quirks::set_hostname(url, &text)),
        "quirks_port" => unit_result(quirks::set_port(url, &text)),
        "quirks_pathname" => {
            quirks::set_pathname(url, &text);
            json!("ok")
        }
        "quirks_search" => {
            quirks::set_search(url, &text);
            json!("ok")
        }
        "quirks_hash" => {
            quirks::set_hash(url, &text);
            json!("ok")
        }
        "path_segments" => {
            let Ok(mut segments) = url.path_segments_mut() else {
                return json!("err");
            };
            for call in step.get("calls").and_then(Value::as_array).into_iter().flatten() {
                match crate::text(call, "call") {
                    "clear" => {
                        segments.clear();
                    }
                    "pop" => {
                        segments.pop();
                    }
                    "pop_if_empty" => {
                        segments.pop_if_empty();
                    }
                    "push" => {
                        segments.push(crate::text(call, "segment"));
                    }
                    "extend" => {
                        let list: Vec<&str> = call
                            .get("segments")
                            .and_then(Value::as_array)
                            .into_iter()
                            .flatten()
                            .filter_map(Value::as_str)
                            .collect();
                        segments.extend(list);
                    }
                    other => panic!("unknown path_segments call {other}"),
                }
            }
            json!("ok")
        }
        "query_pairs" => {
            let mut pairs = url.query_pairs_mut();
            for call in step.get("calls").and_then(Value::as_array).into_iter().flatten() {
                match crate::text(call, "call") {
                    "clear" => {
                        pairs.clear();
                    }
                    "append_pair" => {
                        pairs.append_pair(crate::text(call, "name"), crate::text(call, "value"));
                    }
                    "append_key_only" => {
                        pairs.append_key_only(crate::text(call, "name"));
                    }
                    "extend_pairs" => {
                        let list: Vec<(String, String)> = call
                            .get("pairs")
                            .and_then(Value::as_array)
                            .into_iter()
                            .flatten()
                            .map(|pair| (pair[0].as_str().unwrap().to_owned(), pair[1].as_str().unwrap().to_owned()))
                            .collect();
                        pairs.extend_pairs(list);
                    }
                    "extend_keys_only" => {
                        let list: Vec<String> = call
                            .get("keys")
                            .and_then(Value::as_array)
                            .into_iter()
                            .flatten()
                            .filter_map(Value::as_str)
                            .map(str::to_owned)
                            .collect();
                        pairs.extend_keys_only::<_, String>(list);
                    }
                    "finish" => {
                        pairs.finish();
                    }
                    other => panic!("unknown query_pairs call {other}"),
                }
            }
            json!("ok")
        }
        other => panic!("unknown setter {other}"),
    }
}

fn violation_names(input: &str, base: Option<&Url>) -> Value {
    let seen = RefCell::new(Vec::new());
    let callback = |violation: url::SyntaxViolation| {
        seen.borrow_mut().push(json!([format!("{violation:?}"), violation.to_string(), violation.description()]));
    };
    let result = Url::options().base_url(base).syntax_violation_callback(Some(&callback)).parse(input);
    let href = match result {
        Ok(url) => json!(url.as_str()),
        Err(error) => error_value(error),
    };
    json!({ "violations": seen.into_inner(), "result": href })
}

/// FNV-1a over the replies of a batch, so a test can cover every code point
/// without a corpus of every answer.
fn fnv(digest: &mut u64, bytes: &[u8]) {
    for byte in bytes {
        *digest ^= u64::from(*byte);
        *digest = digest.wrapping_mul(0x100000001b3);
    }
    *digest ^= 0xff;
    *digest = digest.wrapping_mul(0x100000001b3);
}

/// `0000777` as `0*4 7*3 `: most scalar values are in long runs of one class.
fn run_lengths(classes: &str) -> String {
    let mut out = String::new();
    let mut chars = classes.chars().peekable();
    while let Some(class) = chars.next() {
        let mut count = 1;
        while chars.peek() == Some(&class) {
            chars.next();
            count += 1;
        }
        out.push_str(&format!("{class}*{count} "));
    }
    out
}

fn host_text(input: &str) -> String {
    match Host::parse(input) {
        Ok(host) => format!("+{host}"),
        Err(error) => format!("-{error:?}"),
    }
}

pub fn handle(request: &Value) -> Value {
    match crate::text(request, "op") {
        "parse" => {
            let input = crate::text(request, "input");
            match option_text(request, "base") {
                Some(base) => match Url::parse(&base) {
                    Ok(base) => parse_result(base.join(input), request),
                    Err(error) => json!({ "base": error_value(error) }),
                },
                None => parse_result(Url::parse(input), request),
            }
        }
        "violations" => {
            let input = crate::text(request, "input");
            match option_text(request, "base") {
                Some(base) => match Url::parse(&base) {
                    Ok(base) => violation_names(input, Some(&base)),
                    Err(error) => json!({ "base": error_value(error) }),
                },
                None => violation_names(input, None),
            }
        }
        "encoding_override" => {
            // The override upper-cases the query, which shows where it is
            // applied and where it is not.
            let input = crate::text(request, "input");
            fn encode(text: &str) -> std::borrow::Cow<'_, [u8]> {
                text.to_ascii_uppercase().into_bytes().into()
            }
            parse_result(Url::options().encoding_override(Some(&encode)).parse(input), request)
        }
        "parse_with_params" => {
            let pairs: Vec<(String, String)> = request["params"]
                .as_array()
                .unwrap()
                .iter()
                .map(|pair| (pair[0].as_str().unwrap().to_owned(), pair[1].as_str().unwrap().to_owned()))
                .collect();
            parse_result(Url::parse_with_params(crate::text(request, "input"), pairs), request)
        }
        "set" => {
            let mut url = match Url::parse(crate::text(request, "href")) {
                Ok(url) => url,
                Err(error) => return json!({ "href": error_value(error) }),
            };
            let mut results = Vec::new();
            let mut states = Vec::new();
            for step in request["steps"].as_array().unwrap() {
                results.push(apply_setter(&mut url, step));
                states.push(json!(url.as_str()));
            }
            if request["brief"].as_bool().unwrap_or(false) {
                json!({ "results": results, "states": states, "final": brief(&url) })
            } else {
                json!({ "results": results, "states": states, "final": dump(&url) })
            }
        }
        "make_relative" => {
            let base = Url::parse(crate::text(request, "base"));
            let url = Url::parse(crate::text(request, "url"));
            match (base, url) {
                (Ok(base), Ok(url)) => json!({ "relative": base.make_relative(&url) }),
                _ => json!({ "unparsed": true }),
            }
        }
        "host_parse" => {
            let input = crate::text(request, "input");
            let result = if request["opaque"].as_bool().unwrap_or(false) {
                Host::parse_opaque(input)
            } else {
                Host::parse(input)
            };
            match result {
                Ok(host) => json!({ "ok": host_value(&host), "display": host.to_string() }),
                Err(error) => error_value(error),
            }
        }
        "domain" => {
            let input = crate::text(request, "input");
            json!({
                "ascii": quirks::domain_to_ascii(input),
                "unicode": quirks::domain_to_unicode(input),
                "host": host_text(input),
            })
        }
        "host_digest" => {
            // Host::parse of `prefix + char + suffix` for every scalar value
            // in a range, as one digest.
            let prefix = crate::text(request, "prefix");
            let suffix = crate::text(request, "suffix");
            let start = request["start"].as_u64().unwrap() as u32;
            let end = request["end"].as_u64().unwrap() as u32;
            let mut digest = 0xcbf29ce484222325u64;
            let mut text = String::new();
            for code in start..end {
                let Some(c) = char::from_u32(code) else { continue };
                text.clear();
                text.push_str(prefix);
                text.push(c);
                text.push_str(suffix);
                fnv(&mut digest, host_text(&text).as_bytes());
            }
            json!({ "digest": format!("{digest:016x}") })
        }
        "host_list" => {
            // The same as host_digest, with every answer, to find where a
            // digest differs.
            let prefix = crate::text(request, "prefix");
            let suffix = crate::text(request, "suffix");
            let start = request["start"].as_u64().unwrap() as u32;
            let end = request["end"].as_u64().unwrap() as u32;
            let mut answers = Vec::new();
            for code in start..end {
                let Some(c) = char::from_u32(code) else { continue };
                answers.push(host_text(&format!("{prefix}{c}{suffix}")));
            }
            json!({ "answers": answers })
        }
        "form_parse" => {
            let input = unhex(crate::text(request, "hex"));
            let pairs: Vec<Value> = form_urlencoded::parse(&input).map(|(name, value)| json!([name, value])).collect();
            let owned: Vec<Value> =
                form_urlencoded::parse(&input).into_owned().map(|(name, value)| json!([name, value])).collect();
            json!({ "pairs": pairs, "owned": owned, "count": form_urlencoded::parse(&input).count() })
        }
        "byte_serialize" => {
            let input = unhex(crate::text(request, "hex"));
            let pieces: Vec<&str> = form_urlencoded::byte_serialize(&input).collect();
            json!({ "pieces": pieces, "text": form_urlencoded::byte_serialize(&input).collect::<String>() })
        }
        "form_serialize" => {
            let mut serializer = form_urlencoded::Serializer::for_suffix(
                crate::text(request, "target").to_owned(),
                request["start"].as_u64().unwrap_or(0) as usize,
            );
            for call in request["calls"].as_array().unwrap() {
                match crate::text(call, "call") {
                    "clear" => {
                        serializer.clear();
                    }
                    "append_pair" => {
                        serializer.append_pair(crate::text(call, "name"), crate::text(call, "value"));
                    }
                    "append_key_only" => {
                        serializer.append_key_only(crate::text(call, "name"));
                    }
                    "finish" => {
                        serializer.finish();
                    }
                    other => panic!("unknown form_serialize call {other}"),
                }
            }
            json!({ "text": serializer.finish() })
        }
        "from_file_path" => {
            let path = unhex(crate::text(request, "hex"));
            #[cfg(unix)]
            let path: PathBuf = {
                use std::os::unix::ffi::OsStringExt;
                std::ffi::OsString::from_vec(path).into()
            };
            #[cfg(not(unix))]
            let path: PathBuf = String::from_utf8(path).unwrap().into();
            let file = Url::from_file_path(Path::new(&path));
            let directory = Url::from_directory_path(Path::new(&path));
            json!({
                "file": file.map(|url| url.as_str().to_owned()).ok(),
                "directory": directory.map(|url| url.as_str().to_owned()).ok(),
            })
        }
        "to_file_path" => match Url::parse(crate::text(request, "input")) {
            Ok(url) => {
                #[cfg(unix)]
                let path = {
                    use std::os::unix::ffi::OsStrExt;
                    url.to_file_path().ok().map(|path| hex(path.as_os_str().as_bytes()))
                };
                #[cfg(not(unix))]
                let path = url.to_file_path().ok().map(|path| hex(path.to_string_lossy().as_bytes()));
                json!({ "path": path })
            }
            Err(error) => error_value(error),
        },
        "serde" => {
            let input = crate::text(request, "input");
            // What the Serialize impl hands to the serializer, and what the
            // Deserialize impl answers for a string: the URL, or its custom
            // message (serde_json appends " at line L column C" to it).
            let serialized = Url::parse(input).ok().map(|url| serde_json::to_value(&url).unwrap());
            let deserialized = match serde_json::from_str::<Url>(&serde_json::to_string(input).unwrap()) {
                Ok(url) => json!({ "ok": url.as_str() }),
                Err(error) => {
                    let message = error.to_string();
                    let end = message.rfind(" at line ").unwrap_or(message.len());
                    json!({ "err": &message[..end] })
                }
            };
            let wrong_type = serde_json::from_str::<Url>("17").unwrap_err().to_string();
            json!({ "serialized": serialized, "deserialized": deserialized, "wrong_type": wrong_type })
        }
        "debug" => match Url::parse(crate::text(request, "input")) {
            Ok(url) => json!({
                "debug": format!("{url:?}"),
                "pretty": format!("{url:#?}"),
                "padded": format!("{url:>60}|{url:<60}|{url:^7.5}"),
                "host": url.host().map(|host| format!("{host:?}|{host:>30}|{host:<8.3}")),
                "origin": format!("{:?}", url.origin()).starts_with("Tuple"),
            }),
            Err(error) => json!({
                "err": format!("{error:?}"),
                "padded": format!("{error:>50}|{error:<12.6}"),
            }),
        },
        "joining_probe" => {
            // Which of the ContextJ conditions each scalar value satisfies,
            // found by asking the IDNA code itself (see the Go generator).
            let start = request["start"].as_u64().unwrap() as u32;
            let end = request["end"].as_u64().unwrap() as u32;
            let accepts = |text: &str| Host::parse(text).is_ok();
            let mut classes = String::new();
            for code in start..end {
                let Some(c) = char::from_u32(code) else {
                    classes.push('-');
                    continue;
                };
                // Dual-joining bases: one left-to-right (Mongolian), one
                // right-to-left (Arabic), so that the Bidi rule does not
                // reject a label for the direction of `c`.
                let mut bits = 0u8;
                for base in ['\u{1820}', '\u{0628}'] {
                    // c is left- or dual-joining: it may precede ZWNJ.
                    if accepts(&format!("{c}\u{200C}{base}")) {
                        bits |= 1;
                    }
                    // c is right- or dual-joining: it may follow ZWNJ.
                    if accepts(&format!("{base}\u{200C}{c}")) {
                        bits |= 2;
                    }
                    // c is transparent (or left/dual-joining) between a
                    // joining character and a transparent mark before ZWNJ.
                    if accepts(&format!("{base}{c}\u{0301}\u{200C}{base}")) {
                        bits |= 4;
                    }
                }
                classes.push(char::from(b'0' + bits));
            }
            json!({ "classes": run_lengths(&classes) })
        }
        _ => crate::unknown(request),
    }
}
