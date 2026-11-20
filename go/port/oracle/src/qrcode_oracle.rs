//! Oracle module `qrcode`. Requests are objects with an `"op"` member; see the
//! Go tests in go/crates/qrcode that use this module for the operations it
//! serves.
//!
//! The data to encode is given either as `"data_hex"` or as `"gen"`, a
//! seeded generator both sides expand the same way (`generate`), so that a
//! recorded case of a few kilobytes of data is a few bytes of request.

use qrcode::bits::{Bits, ExtendedMode};
use qrcode::canvas::{Canvas, MaskPattern};
use qrcode::optimize::{Optimizer, Parser, Segment, total_encoded_len};
use qrcode::types::{Color, EcLevel, Mode, QrError, Version};
use qrcode::{QrCode, ec};
use serde_json::{Value, json};

fn hex(bytes: &[u8]) -> String {
    bytes.iter().map(|byte| format!("{byte:02x}")).collect()
}

fn unhex(text: &str) -> Vec<u8> {
    (0..text.len() / 2)
        .map(|index| u8::from_str_radix(&text[2 * index..2 * index + 2], 16).unwrap())
        .collect()
}

/// The 64-bit LCG of the Go test's `generate`: the next state, and 31 bits
/// of it.
fn next(state: &mut u64) -> u64 {
    *state = state.wrapping_mul(6364136223846793005).wrapping_add(1442695040888963407);
    *state >> 33
}

const NUMERIC: &[u8] = b"0123456789";
const ALPHANUMERIC: &[u8] = b"0123456789ABCDEFGHIJKLMNOPQRSTUVWXYZ $%*+-./:";
const URL: &[u8] = b"abcdefghijklmnopqrstuvwxyz0123456789-._~:/?#[]@!$&'()*+,;=%ABCXYZ";

/// `count` bytes of one kind of data, from `seed`.
fn generate(kind: &str, count: usize, seed: u64) -> Vec<u8> {
    let mut state = seed ^ 0x9e3779b97f4a7c15;
    let mut out = Vec::with_capacity(count);
    let pick = |state: &mut u64, alphabet: &[u8]| alphabet[(next(state) % alphabet.len() as u64) as usize];
    while out.len() < count {
        match kind {
            "numeric" => out.push(pick(&mut state, NUMERIC)),
            "alphanumeric" => out.push(pick(&mut state, ALPHANUMERIC)),
            "url" => out.push(pick(&mut state, URL)),
            "bytes" => out.push((next(&mut state) & 0xff) as u8),
            "kanji" => {
                // A Shift JIS double-byte character: a lead byte in
                // 0x81..=0x9f or 0xe0..=0xeb, a trail byte in 0x40..=0xfc
                // other than 0x7f.
                let lead = next(&mut state) % 43;
                let lead = if lead < 31 { 0x81 + lead } else { 0xe0 + (lead - 31) };
                let mut trail = 0x40 + next(&mut state) % 0xbd;
                if trail == 0x7f {
                    trail = 0x80;
                }
                out.push(lead as u8);
                out.push(trail as u8);
            }
            "mixed" => {
                // Runs of the other kinds, so the segment optimizer has
                // switches to weigh.
                let kinds = ["numeric", "alphanumeric", "url", "bytes", "kanji"];
                let kind = kinds[(next(&mut state) % kinds.len() as u64) as usize];
                let run = 1 + (next(&mut state) % 24) as usize;
                let sub_seed = next(&mut state);
                out.extend(generate(kind, run, sub_seed));
            }
            other => panic!("unknown data kind {other}"),
        }
    }
    out.truncate(count);
    out
}

fn data_of(request: &Value) -> Vec<u8> {
    if let Some(spec) = request.get("gen") {
        generate(
            spec["kind"].as_str().unwrap(),
            spec["len"].as_u64().unwrap() as usize,
            spec["seed"].as_u64().unwrap(),
        )
    } else {
        unhex(crate::text(request, "data_hex"))
    }
}

fn level_of(value: &Value) -> Option<EcLevel> {
    match value.as_str()? {
        "L" => Some(EcLevel::L),
        "M" => Some(EcLevel::M),
        "Q" => Some(EcLevel::Q),
        "H" => Some(EcLevel::H),
        other => panic!("unknown level {other}"),
    }
}

fn version_of(value: &Value) -> Option<Version> {
    if let Some(n) = value.get("normal") {
        return Some(Version::Normal(n.as_i64().unwrap() as i16));
    }
    if let Some(n) = value.get("micro") {
        return Some(Version::Micro(n.as_i64().unwrap() as i16));
    }
    None
}

fn pattern_of(name: &str) -> MaskPattern {
    match name {
        "Checkerboard" => MaskPattern::Checkerboard,
        "HorizontalLines" => MaskPattern::HorizontalLines,
        "VerticalLines" => MaskPattern::VerticalLines,
        "DiagonalLines" => MaskPattern::DiagonalLines,
        "LargeCheckerboard" => MaskPattern::LargeCheckerboard,
        "Fields" => MaskPattern::Fields,
        "Diamonds" => MaskPattern::Diamonds,
        "Meadow" => MaskPattern::Meadow,
        other => panic!("unknown mask pattern {other}"),
    }
}

fn mode_name(mode: Mode) -> String {
    format!("{mode:?}")
}

fn error_value(error: QrError) -> Value {
    json!({
        "err": format!("{error:?}"),
        "display": error.to_string(),
        "padded": format!("{error:>30}|{error:<4.2}"),
        "context": format!("encode web login QR: {error}"),
    })
}

/// FNV-1a over a sequence of values, so a reply can stand for a whole matrix.
fn fnv(values: impl Iterator<Item = u8>) -> String {
    let mut digest = 0xcbf29ce484222325u64;
    for value in values {
        digest ^= u64::from(value);
        digest = digest.wrapping_mul(0x100000001b3);
    }
    format!("{digest:016x}")
}

/// The modules as text, one row a line, `#` for dark.
fn rows(colors: &[Color], width: usize) -> Vec<String> {
    colors
        .chunks(width)
        .map(|row| row.iter().map(|color| if *color == Color::Dark { '#' } else { '.' }).collect())
        .collect()
}

fn code_value(code: &QrCode, full: bool) -> Value {
    let colors = code.to_colors();
    let width = code.width();
    let mut functional = Vec::with_capacity(width * width);
    for y in 0..width {
        for x in 0..width {
            functional.push(u8::from(code.is_functional(x, y)));
        }
    }
    let render: String = code.render::<char>().build();
    let render_str: String = code.render::<&str>().dark_color("##").light_color("  ").module_dimensions(2, 1).build();
    let mut value = json!({
        "version": format!("{:?}", code.version()),
        "level": format!("{:?}", code.error_correction_level()),
        "width": width,
        "max_allowed_errors": code.max_allowed_errors(),
        "modules": fnv(colors.iter().map(|color| *color as u8)),
        "functional": fnv(functional.into_iter()),
        "render": fnv(render.bytes()),
        "render_str": fnv(render_str.bytes()),
        "corner": code[(width - 1, width - 1)] == Color::Dark,
    });
    if full {
        value["rows"] = json!(rows(&colors, width));
    }
    value
}

fn bits_value(bits: &Bits) -> Value {
    json!({ "len": bits.len(), "empty": bits.is_empty() })
}

pub fn handle(request: &Value) -> Value {
    match crate::text(request, "op") {
        "encode" => {
            let data = data_of(request);
            let level = level_of(&request["level"]);
            let version = version_of(&request["version"]);
            let result = match (version, level) {
                (Some(version), Some(level)) => QrCode::with_version(&data, version, level),
                (None, Some(level)) => QrCode::with_error_correction_level(&data, level),
                (None, None) => QrCode::new(&data),
                (Some(_), None) => panic!("a version needs a level"),
            };
            match result {
                Ok(code) => json!({ "ok": code_value(&code, request["full"].as_bool().unwrap_or(false)) }),
                Err(error) => error_value(error),
            }
        }
        "bits" => {
            // A sequence of pushes onto one Bits, and what each answered.
            let version = version_of(&request["version"]).unwrap();
            let mut bits = Bits::new(version);
            let mut results = Vec::new();
            for call in request["calls"].as_array().unwrap() {
                let result = match crate::text(call, "push") {
                    "numeric" => bits.push_numeric_data(&data_of(call)),
                    "alphanumeric" => bits.push_alphanumeric_data(&data_of(call)),
                    "byte" => bits.push_byte_data(&data_of(call)),
                    "kanji" => bits.push_kanji_data(&data_of(call)),
                    "optimal" => bits.push_optimal_data(&data_of(call)),
                    "eci" => bits.push_eci_designator(call["value"].as_u64().unwrap() as u32),
                    "fnc1_first" => bits.push_fnc1_first_position(),
                    "fnc1_second" => bits.push_fnc1_second_position(call["value"].as_u64().unwrap() as u8),
                    "structured_append" => bits.push_mode_indicator(ExtendedMode::StructuredAppend),
                    "terminator" => bits.push_terminator(level_of(&call["level"]).unwrap()),
                    other => panic!("unknown push {other}"),
                };
                results.push(match result {
                    Ok(()) => json!("ok"),
                    Err(error) => json!(format!("{error:?}")),
                });
                results.push(bits_value(&bits));
            }
            let max_len = ["L", "M", "Q", "H"]
                .iter()
                .map(|level| match bits.max_len(level_of(&json!(level)).unwrap()) {
                    Ok(len) => json!(len),
                    Err(error) => json!(format!("{error:?}")),
                })
                .collect::<Vec<_>>();
            json!({ "results": results, "max_len": max_len, "bytes": hex(&bits.into_bytes()) })
        }
        "segments" => {
            let data = data_of(request);
            let version = version_of(&request["version"]).unwrap();
            let as_list = |segments: &[Segment]| {
                segments.iter().map(|s| json!([mode_name(s.mode), s.begin, s.end])).collect::<Vec<_>>()
            };
            let parsed = Parser::new(&data).collect::<Vec<Segment>>();
            let optimized = Optimizer::new(parsed.iter().copied(), version).collect::<Vec<_>>();
            let lens = parsed.iter().map(|s| s.encoded_len(version)).collect::<Vec<_>>();
            json!({
                "parsed": as_list(&parsed),
                "optimized": as_list(&optimized),
                "parsed_lens": lens,
                "total": total_encoded_len(&optimized, version),
            })
        }
        "encode_auto" => {
            let data = data_of(request);
            match qrcode::bits::encode_auto(&data, level_of(&request["level"]).unwrap()) {
                Ok(bits) => {
                    let version = format!("{:?}", bits.version());
                    let len = bits.len();
                    json!({ "version": version, "len": len, "bytes": fnv(bits.into_bytes().into_iter()) })
                }
                Err(error) => json!({ "err": format!("{error:?}") }),
            }
        }
        "codewords" => {
            let data = data_of(request);
            let version = version_of(&request["version"]).unwrap();
            match ec::construct_codewords(&data, version, level_of(&request["level"]).unwrap()) {
                // Digests, with the first bytes to show where a difference
                // starts.
                Ok((blocks, ec)) => json!({
                    "data": fnv(blocks.iter().copied()),
                    "data_head": hex(&blocks[..blocks.len().min(12)]),
                    "data_len": blocks.len(),
                    "ec": fnv(ec.iter().copied()),
                    "ec_head": hex(&ec[..ec.len().min(12)]),
                    "ec_len": ec.len(),
                }),
                Err(error) => json!({ "err": format!("{error:?}") }),
            }
        }
        "ecc" => {
            let data = data_of(request);
            let size = request["size"].as_u64().unwrap() as usize;
            json!({ "ec": hex(&ec::create_error_correction_code(&data, size)) })
        }
        "max_allowed_errors" => {
            let version = version_of(&request["version"]).unwrap();
            match ec::max_allowed_errors(version, level_of(&request["level"]).unwrap()) {
                Ok(errors) => json!({ "ok": errors }),
                Err(error) => json!({ "err": format!("{error:?}") }),
            }
        }
        "mask" => {
            // One mask applied to a canvas holding the given data and error
            // correction bytes, as with_bits draws them.
            let version = version_of(&request["version"]).unwrap();
            let level = level_of(&request["level"]).unwrap();
            let data = data_of(request);
            let ec = data_of(&request["ec"]);
            let mut canvas = Canvas::new(version, level);
            canvas.draw_all_functional_patterns();
            canvas.draw_data(&data, &ec);
            #[allow(deprecated)]
            let bools = canvas.to_bools();
            let best = canvas.apply_best_mask().into_colors();
            canvas.apply_mask(pattern_of(crate::text(request, "pattern")));
            let colors = canvas.into_colors();
            let width = version.width() as usize;
            let mut value = json!({
                "unmasked": fnv(bools.into_iter().map(u8::from)),
                "masked": fnv(colors.iter().map(|color| *color as u8)),
                "best": fnv(best.iter().map(|color| *color as u8)),
            });
            if request["full"].as_bool().unwrap_or(false) {
                value["rows"] = json!(rows(&colors, width));
            }
            value
        }
        _ => crate::unknown(request),
    }
}
