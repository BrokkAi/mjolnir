//! Oracle module `std_num`: the number methods of core and std that the Go
//! package `go/rs` reproduces (num_*.go). The Go tests that drive it are
//! `go/rs/num_oracle_test.go`.
//!
//! Integers travel as decimal strings and floats as the decimal string of
//! their bits, so no JSON number is ever rounded on the way. A float result
//! that is NaN is replied as `"nan"`: Rust does not fix the bits of a NaN it
//! computes.
//!
//! Requests:
//!
//! - `{"op":"int","ty":T,"f":method,"a":..,"b":..,"n":u32,"mode":..}` calls
//!   an integer method. `mode` is `"debug"` or `"release"` for the methods
//!   whose overflow behaviour follows the build profile; the oracle is a
//!   release build, so the debug behaviour is taken from the `strict_`
//!   methods, which panic with the same messages.
//! - `{"op":"usize63","f":method,"a":..,"b":..}` calls a usize method and
//!   applies the port's rule for usize (an int, so never above isize::MAX).
//! - `{"op":"try_from","from":S,"to":T,"a":..}`
//! - `{"op":"from_str_radix","ty":T,"s":..,"radix":u32}`
//! - `{"op":"bytes","ty":T,"f":"to_le"|..|"from_ne","a":..|"hex":..}`
//! - `{"op":"float_to_int","from":F,"to":T,"bits":..}`, `{"op":"int_to_float",..}`,
//!   `{"op":"float_to_float","from":F,"bits":..}`
//! - `{"op":"float","ty":F,"f":method,"a":bits,"b":bits,"c":bits,"n":i32}`
//!   calls a float method whose result is exact.
//! - `{"op":"libm","ty":F,"f":method,"a":bits,"b":bits,"go":bits,"tol":n}`
//!   calls a method that Rust delegates to the C library and says whether
//!   the Go result it was given is within `tol` units in the last place.
//! - `{"op":"wide_text","ty":"u128"|"i128","a":..}` formats in every base.
//! - `{"op":"consts"}`

use std::hint::black_box;

use serde_json::{Value, json};

fn text<'a>(request: &'a Value, key: &str) -> &'a str {
    crate::text(request, key)
}

fn number_u32(request: &Value, key: &str) -> u32 {
    request.get(key).and_then(Value::as_u64).unwrap_or(0) as u32
}

fn number_i32(request: &Value, key: &str) -> i32 {
    request.get(key).and_then(Value::as_i64).unwrap_or(0) as i32
}

/// An integer argument: a decimal string, or zero when the member is absent.
fn int<T: std::str::FromStr + Default>(request: &Value, key: &str) -> T
where
    T::Err: std::fmt::Debug,
{
    match request.get(key).and_then(Value::as_str) {
        Some(text) => text.parse().expect("integer argument"),
        None => T::default(),
    }
}

fn val<T: ToString>(value: T) -> Value {
    json!({ "r": value.to_string() })
}

fn opt<T: ToString>(value: Option<T>) -> Value {
    json!({ "r": value.map(|v| v.to_string()) })
}

fn ovf<T: ToString>(value: (T, bool)) -> Value {
    json!({ "r": value.0.to_string(), "o": value.1 })
}

fn flag(value: bool) -> Value {
    json!({ "r": value })
}

fn debug_mode(request: &Value) -> bool {
    match text(request, "mode") {
        "debug" => true,
        "release" => false,
        other => panic!("oracle: mode must be debug or release, not {other:?}"),
    }
}

/// The methods every integer type has.
macro_rules! common_int_methods {
    ($t:ty, $f:expr, $request:expr) => {{
        let request: &Value = $request;
        let a: $t = black_box(int(request, "a"));
        let b: $t = black_box(int(request, "b"));
        let c: $t = black_box(int(request, "c"));
        let n: u32 = black_box(number_u32(request, "n"));
        Some(match $f {
            "checked_add" => opt(a.checked_add(b)),
            "checked_sub" => opt(a.checked_sub(b)),
            "checked_mul" => opt(a.checked_mul(b)),
            "checked_div" => opt(a.checked_div(b)),
            "checked_rem" => opt(a.checked_rem(b)),
            "checked_div_euclid" => opt(a.checked_div_euclid(b)),
            "checked_rem_euclid" => opt(a.checked_rem_euclid(b)),
            "checked_neg" => opt(a.checked_neg()),
            "checked_pow" => opt(a.checked_pow(n)),
            "checked_shl" => opt(a.checked_shl(n)),
            "checked_shr" => opt(a.checked_shr(n)),
            "checked_ilog2" => opt(a.checked_ilog2()),
            "checked_ilog10" => opt(a.checked_ilog10()),
            "checked_ilog" => opt(a.checked_ilog(b)),
            "saturating_add" => val(a.saturating_add(b)),
            "saturating_sub" => val(a.saturating_sub(b)),
            "saturating_mul" => val(a.saturating_mul(b)),
            "saturating_pow" => val(a.saturating_pow(n)),
            "saturating_div" => val(a.saturating_div(b)),
            "wrapping_add" => val(a.wrapping_add(b)),
            "wrapping_sub" => val(a.wrapping_sub(b)),
            "wrapping_mul" => val(a.wrapping_mul(b)),
            "wrapping_neg" => val(a.wrapping_neg()),
            "wrapping_pow" => val(a.wrapping_pow(n)),
            "wrapping_shl" => val(a.wrapping_shl(n)),
            "wrapping_shr" => val(a.wrapping_shr(n)),
            "wrapping_div" => val(a.wrapping_div(b)),
            "wrapping_rem" => val(a.wrapping_rem(b)),
            "overflowing_add" => ovf(a.overflowing_add(b)),
            "overflowing_sub" => ovf(a.overflowing_sub(b)),
            "overflowing_mul" => ovf(a.overflowing_mul(b)),
            "overflowing_neg" => ovf(a.overflowing_neg()),
            "overflowing_pow" => ovf(a.overflowing_pow(n)),
            "overflowing_shl" => ovf(a.overflowing_shl(n)),
            "overflowing_shr" => ovf(a.overflowing_shr(n)),
            "overflowing_div" => ovf(a.overflowing_div(b)),
            "overflowing_rem" => ovf(a.overflowing_rem(b)),
            "abs_diff" => val(a.abs_diff(b)),
            "pow" => {
                if debug_mode(request) {
                    val(a.strict_pow(n))
                } else {
                    val(a.wrapping_pow(n))
                }
            }
            "isqrt" => val(a.isqrt()),
            "ilog2" => val(a.ilog2()),
            "ilog10" => val(a.ilog10()),
            "ilog" => val(a.ilog(b)),
            "div_euclid" => val(a.div_euclid(b)),
            "rem_euclid" => val(a.rem_euclid(b)),
            "clamp" => val(a.clamp(b, c)),
            "count_ones" => val(a.count_ones()),
            "count_zeros" => val(a.count_zeros()),
            "leading_zeros" => val(a.leading_zeros()),
            "trailing_zeros" => val(a.trailing_zeros()),
            "leading_ones" => val(a.leading_ones()),
            "trailing_ones" => val(a.trailing_ones()),
            "rotate_left" => val(a.rotate_left(n)),
            "rotate_right" => val(a.rotate_right(n)),
            "swap_bytes" => val(a.swap_bytes()),
            "reverse_bits" => val(a.reverse_bits()),
            // The operators, for the 128-bit types that the port holds as
            // structs. Division panics on its own in every profile.
            "add" => {
                if debug_mode(request) {
                    val(a.strict_add(b))
                } else {
                    val(a.wrapping_add(b))
                }
            }
            "sub" => {
                if debug_mode(request) {
                    val(a.strict_sub(b))
                } else {
                    val(a.wrapping_sub(b))
                }
            }
            "mul" => {
                if debug_mode(request) {
                    val(a.strict_mul(b))
                } else {
                    val(a.wrapping_mul(b))
                }
            }
            "div" => val(a / b),
            "rem" => val(a % b),
            "shl" => {
                if debug_mode(request) {
                    val(a.strict_shl(n))
                } else {
                    val(a.wrapping_shl(n))
                }
            }
            "shr" => {
                if debug_mode(request) {
                    val(a.strict_shr(n))
                } else {
                    val(a.wrapping_shr(n))
                }
            }
            "bitand" => val(a & b),
            "bitor" => val(a | b),
            "bitxor" => val(a ^ b),
            "not" => val(!a),
            "cmp" => json!({ "r": a.cmp(&b) as i32 }),
            "min" => val(a.min(b)),
            "max" => val(a.max(b)),
            _ => return None,
        })
    }};
}

macro_rules! unsigned_type {
    ($name:ident, $t:ty, $signed:ty) => {
        fn $name(f: &str, request: &Value) -> Option<Value> {
            fn common(f: &str, request: &Value) -> Option<Value> {
                common_int_methods!($t, f, request)
            }
            if let Some(reply) = common(f, request) {
                return Some(reply);
            }
            let a: $t = black_box(int(request, "a"));
            let b: $t = black_box(int(request, "b"));
            let s: $signed = black_box(int(request, "s"));
            Some(match f {
                "checked_next_power_of_two" => opt(a.checked_next_power_of_two()),
                "checked_next_multiple_of" => opt(a.checked_next_multiple_of(b)),
                "checked_add_signed" => opt(a.checked_add_signed(s)),
                "checked_sub_signed" => opt(a.checked_sub_signed(s)),
                "saturating_add_signed" => val(a.saturating_add_signed(s)),
                "saturating_sub_signed" => val(a.saturating_sub_signed(s)),
                "wrapping_add_signed" => val(a.wrapping_add_signed(s)),
                "div_ceil" => val(a.div_ceil(b)),
                "is_power_of_two" => flag(a.is_power_of_two()),
                "is_multiple_of" => flag(a.is_multiple_of(b)),
                "next_power_of_two" => match a.checked_next_power_of_two() {
                    Some(power) => val(power),
                    // `one_less_than_next_power_of_two() + 1` overflows.
                    None if debug_mode(request) => panic!("attempt to add with overflow"),
                    None => val(0),
                },
                "next_multiple_of" => {
                    if b == 0 || a.checked_next_multiple_of(b).is_some() {
                        val(a.next_multiple_of(b))
                    } else if debug_mode(request) {
                        // `self + (rhs - r)` overflows.
                        panic!("attempt to add with overflow")
                    } else {
                        val(a.wrapping_add(b - a % b))
                    }
                }
                _ => return None,
            })
        }
    };
}

macro_rules! signed_type {
    ($name:ident, $t:ty, $unsigned:ty) => {
        fn $name(f: &str, request: &Value) -> Option<Value> {
            fn common(f: &str, request: &Value) -> Option<Value> {
                common_int_methods!($t, f, request)
            }
            if let Some(reply) = common(f, request) {
                return Some(reply);
            }
            let a: $t = black_box(int(request, "a"));
            let u: $unsigned = black_box(int(request, "u"));
            Some(match f {
                "checked_abs" => opt(a.checked_abs()),
                "checked_isqrt" => opt(a.checked_isqrt()),
                "checked_add_unsigned" => opt(a.checked_add_unsigned(u)),
                "checked_sub_unsigned" => opt(a.checked_sub_unsigned(u)),
                "saturating_add_unsigned" => val(a.saturating_add_unsigned(u)),
                "saturating_sub_unsigned" => val(a.saturating_sub_unsigned(u)),
                "saturating_neg" => val(a.saturating_neg()),
                "saturating_abs" => val(a.saturating_abs()),
                "wrapping_abs" => val(a.wrapping_abs()),
                "overflowing_abs" => ovf(a.overflowing_abs()),
                "unsigned_abs" => val(a.unsigned_abs()),
                "signum" => val(a.signum()),
                "abs" => {
                    if debug_mode(request) {
                        val(a.strict_abs())
                    } else {
                        val(a.wrapping_abs())
                    }
                }
                "neg" => {
                    if debug_mode(request) {
                        val(a.strict_neg())
                    } else {
                        val(a.wrapping_neg())
                    }
                }
                _ => return None,
            })
        }
    };
}

unsigned_type!(u8_method, u8, i8);
unsigned_type!(u16_method, u16, i16);
unsigned_type!(u32_method, u32, i32);
unsigned_type!(u64_method, u64, i64);
unsigned_type!(usize_method, usize, isize);
unsigned_type!(u128_method, u128, i128);
signed_type!(i8_method, i8, u8);
signed_type!(i16_method, i16, u16);
signed_type!(i32_method, i32, u32);
signed_type!(i64_method, i64, u64);
signed_type!(isize_method, isize, usize);
signed_type!(i128_method, i128, u128);

fn int_method(request: &Value) -> Value {
    let f = text(request, "f");
    let reply = match text(request, "ty") {
        "u8" => u8_method(f, request),
        "u16" => u16_method(f, request),
        "u32" => u32_method(f, request),
        "u64" => u64_method(f, request),
        "usize" => usize_method(f, request),
        "u128" => u128_method(f, request),
        "i8" => i8_method(f, request),
        "i16" => i16_method(f, request),
        "i32" => i32_method(f, request),
        "i64" => i64_method(f, request),
        "isize" => isize_method(f, request),
        "i128" => i128_method(f, request),
        _ => None,
    };
    reply.unwrap_or_else(|| crate::unknown(request))
}

/// A usize method under the port's rule: a usize is a Go int, so a result
/// above isize::MAX does not exist. Checked methods are None there and
/// saturating ones stop at isize::MAX. Operands are below 2^63.
fn usize63(request: &Value) -> Value {
    const MAX: usize = isize::MAX as usize;
    let a: usize = black_box(int(request, "a"));
    let b: usize = black_box(int(request, "b"));
    let s: isize = black_box(int(request, "s"));
    let n = black_box(number_u32(request, "n"));
    assert!(a <= MAX && b <= MAX, "oracle: usize63 operands are below 2^63");
    let fits = |value: Option<usize>| opt(value.filter(|v| *v <= MAX));
    let capped = |value: usize| val(value.min(MAX));
    let flagged = |value: (usize, bool)| json!({ "r": (value.0 as isize).to_string(), "o": value.1 || value.0 > MAX });
    match text(request, "f") {
        "checked_add" => fits(a.checked_add(b)),
        "checked_sub" => fits(a.checked_sub(b)),
        "checked_mul" => fits(a.checked_mul(b)),
        "checked_div" => fits(a.checked_div(b)),
        "checked_rem" => fits(a.checked_rem(b)),
        "checked_div_euclid" => fits(a.checked_div_euclid(b)),
        "checked_rem_euclid" => fits(a.checked_rem_euclid(b)),
        "checked_neg" => fits(a.checked_neg()),
        "checked_pow" => fits(a.checked_pow(n)),
        "checked_add_signed" => fits(a.checked_add_signed(s)),
        "checked_sub_signed" => fits(a.checked_sub_signed(s)),
        "checked_next_power_of_two" => fits(a.checked_next_power_of_two()),
        "checked_next_multiple_of" => fits(a.checked_next_multiple_of(b)),
        "checked_ilog2" => opt(a.checked_ilog2()),
        "checked_ilog10" => opt(a.checked_ilog10()),
        "checked_ilog" => opt(a.checked_ilog(b)),
        "saturating_add" => capped(a.saturating_add(b)),
        "saturating_sub" => capped(a.saturating_sub(b)),
        "saturating_mul" => capped(a.saturating_mul(b)),
        "saturating_pow" => capped(a.saturating_pow(n)),
        "saturating_div" => capped(a.saturating_div(b)),
        "saturating_add_signed" => capped(a.saturating_add_signed(s)),
        "saturating_sub_signed" => capped(a.saturating_sub_signed(s)),
        // The wrapping methods keep the 64-bit pattern, which the port
        // holds in an int.
        "wrapping_add" => val(a.wrapping_add(b) as isize),
        "wrapping_sub" => val(a.wrapping_sub(b) as isize),
        "wrapping_mul" => val(a.wrapping_mul(b) as isize),
        "wrapping_neg" => val(a.wrapping_neg() as isize),
        "wrapping_pow" => val(a.wrapping_pow(n) as isize),
        "wrapping_shl" => val(a.wrapping_shl(n) as isize),
        "wrapping_shr" => val(a.wrapping_shr(n) as isize),
        "wrapping_add_signed" => val(a.wrapping_add_signed(s) as isize),
        "checked_shl" => opt(a.checked_shl(n).map(|v| v as isize)),
        "checked_shr" => opt(a.checked_shr(n).map(|v| v as isize)),
        "overflowing_add" => flagged(a.overflowing_add(b)),
        "overflowing_sub" => flagged(a.overflowing_sub(b)),
        "overflowing_mul" => flagged(a.overflowing_mul(b)),
        "abs_diff" => val(a.abs_diff(b)),
        "isqrt" => val(a.isqrt()),
        "ilog2" => val(a.ilog2()),
        "ilog10" => val(a.ilog10()),
        "ilog" => val(a.ilog(b)),
        "div_euclid" => val(a.div_euclid(b)),
        "rem_euclid" => val(a.rem_euclid(b)),
        "div_ceil" => val(a.div_ceil(b)),
        "is_multiple_of" => flag(a.is_multiple_of(b)),
        "is_power_of_two" => flag(a.is_power_of_two()),
        "pow" => match a.checked_pow(n).filter(|v| *v <= MAX) {
            Some(power) => val(power),
            None if debug_mode(request) => panic!("attempt to exponentiate with overflow"),
            None => val(a.wrapping_pow(n) as isize),
        },
        "next_power_of_two" => match a.checked_next_power_of_two().filter(|v| *v <= MAX) {
            Some(power) => val(power),
            None if debug_mode(request) => panic!("attempt to add with overflow"),
            None => val(a.checked_next_power_of_two().unwrap_or(0) as isize),
        },
        "next_multiple_of" => {
            if b == 0 {
                val(a.next_multiple_of(b))
            } else {
                match a.checked_next_multiple_of(b).filter(|v| *v <= MAX) {
                    Some(multiple) => val(multiple),
                    None if debug_mode(request) => panic!("attempt to add with overflow"),
                    None => val(a.wrapping_add(b - a % b) as isize),
                }
            }
        }
        _ => crate::unknown(request),
    }
}

fn try_from_error(error: std::num::TryFromIntError) -> Value {
    json!({ "err": error.to_string(), "debug": format!("{error:?}") })
}

macro_rules! try_from_source {
    ($request:expr, $source:ty, [$($target:ident),*]) => {{
        let value: $source = black_box(int($request, "a"));
        match text($request, "to") {
            $(stringify!($target) => match <$target>::try_from(value) {
                Ok(converted) => val(converted),
                #[allow(unreachable_patterns)]
                Err(error) => try_from_error(error.into()),
            },)*
            _ => crate::unknown($request),
        }
    }};
}

fn try_from(request: &Value) -> Value {
    macro_rules! sources {
        ($($source:ident),*) => {
            match text(request, "from") {
                $(stringify!($source) => try_from_source!(
                    request, $source,
                    [u8, u16, u32, u64, usize, u128, i8, i16, i32, i64, isize, i128]
                ),)*
                _ => crate::unknown(request),
            }
        };
    }
    sources!(u8, u16, u32, u64, usize, u128, i8, i16, i32, i64, isize, i128)
}

fn from_str_radix(request: &Value) -> Value {
    let source = text(request, "s");
    let radix = number_u32(request, "radix");
    macro_rules! types {
        ($($t:ident),*) => {
            match text(request, "ty") {
                $(stringify!($t) => match <$t>::from_str_radix(source, radix) {
                    Ok(value) => val(value),
                    Err(error) => json!({
                        "err": error.to_string(),
                        "kind": format!("{:?}", error.kind()),
                        "debug": format!("{error:?}"),
                    }),
                },)*
                _ => crate::unknown(request),
            }
        };
    }
    types!(u8, u16, u32, u64, usize, u128, i8, i16, i32, i64, isize, i128)
}

fn hex(bytes: &[u8]) -> String {
    bytes.iter().map(|byte| format!("{byte:02x}")).collect()
}

fn unhex(text: &str) -> Vec<u8> {
    (0..text.len() / 2).map(|index| u8::from_str_radix(&text[index * 2..index * 2 + 2], 16).unwrap()).collect()
}

fn bytes(request: &Value) -> Value {
    let f = text(request, "f");
    macro_rules! ints {
        ($($t:ident),*) => {
            match text(request, "ty") {
                $(stringify!($t) => {
                    match f {
                        "to_le" => json!({ "hex": hex(&int::<$t>(request, "a").to_le_bytes()) }),
                        "to_be" => json!({ "hex": hex(&int::<$t>(request, "a").to_be_bytes()) }),
                        "to_ne" => json!({ "hex": hex(&int::<$t>(request, "a").to_ne_bytes()) }),
                        "from_le" => val(<$t>::from_le_bytes(unhex(text(request, "hex")).try_into().unwrap())),
                        "from_be" => val(<$t>::from_be_bytes(unhex(text(request, "hex")).try_into().unwrap())),
                        "from_ne" => val(<$t>::from_ne_bytes(unhex(text(request, "hex")).try_into().unwrap())),
                        _ => crate::unknown(request),
                    }
                })*
                "f32" => {
                    match f {
                        "to_le" => json!({ "hex": hex(&f32::from_bits(int::<u32>(request, "a")).to_le_bytes()) }),
                        "to_be" => json!({ "hex": hex(&f32::from_bits(int::<u32>(request, "a")).to_be_bytes()) }),
                        "to_ne" => json!({ "hex": hex(&f32::from_bits(int::<u32>(request, "a")).to_ne_bytes()) }),
                        "from_le" => val(f32::from_le_bytes(unhex(text(request, "hex")).try_into().unwrap()).to_bits()),
                        "from_be" => val(f32::from_be_bytes(unhex(text(request, "hex")).try_into().unwrap()).to_bits()),
                        "from_ne" => val(f32::from_ne_bytes(unhex(text(request, "hex")).try_into().unwrap()).to_bits()),
                        _ => crate::unknown(request),
                    }
                }
                "f64" => {
                    match f {
                        "to_le" => json!({ "hex": hex(&f64::from_bits(int::<u64>(request, "a")).to_le_bytes()) }),
                        "to_be" => json!({ "hex": hex(&f64::from_bits(int::<u64>(request, "a")).to_be_bytes()) }),
                        "to_ne" => json!({ "hex": hex(&f64::from_bits(int::<u64>(request, "a")).to_ne_bytes()) }),
                        "from_le" => val(f64::from_le_bytes(unhex(text(request, "hex")).try_into().unwrap()).to_bits()),
                        "from_be" => val(f64::from_be_bytes(unhex(text(request, "hex")).try_into().unwrap()).to_bits()),
                        "from_ne" => val(f64::from_ne_bytes(unhex(text(request, "hex")).try_into().unwrap()).to_bits()),
                        _ => crate::unknown(request),
                    }
                }
                _ => crate::unknown(request),
            }
        };
    }
    ints!(u8, u16, u32, u64, usize, u128, i8, i16, i32, i64, isize, i128)
}

fn f32_arg(request: &Value, key: &str) -> f32 {
    black_box(f32::from_bits(int::<u32>(request, key)))
}

fn f64_arg(request: &Value, key: &str) -> f64 {
    black_box(f64::from_bits(int::<u64>(request, key)))
}

fn f32_bits(value: f32) -> Value {
    if value.is_nan() { json!({ "bits": "nan" }) } else { json!({ "bits": value.to_bits().to_string() }) }
}

fn f64_bits(value: f64) -> Value {
    if value.is_nan() { json!({ "bits": "nan" }) } else { json!({ "bits": value.to_bits().to_string() }) }
}

fn float_to_int(request: &Value) -> Value {
    macro_rules! targets {
        ($value:expr, [$($t:ident),*]) => {
            match text(request, "to") {
                $(stringify!($t) => val($value as $t),)*
                _ => crate::unknown(request),
            }
        };
    }
    match text(request, "from") {
        "f32" => {
            let value = f32_arg(request, "bits");
            targets!(value, [u8, u16, u32, u64, usize, u128, i8, i16, i32, i64, isize, i128])
        }
        "f64" => {
            let value = f64_arg(request, "bits");
            targets!(value, [u8, u16, u32, u64, usize, u128, i8, i16, i32, i64, isize, i128])
        }
        _ => crate::unknown(request),
    }
}

fn int_to_float(request: &Value) -> Value {
    macro_rules! sources {
        ($($t:ident),*) => {
            match text(request, "from") {
                $(stringify!($t) => {
                    let value: $t = black_box(int(request, "a"));
                    match text(request, "to") {
                        "f32" => f32_bits(value as f32),
                        "f64" => f64_bits(value as f64),
                        _ => crate::unknown(request),
                    }
                })*
                _ => crate::unknown(request),
            }
        };
    }
    sources!(u8, u16, u32, u64, usize, u128, i8, i16, i32, i64, isize, i128)
}

fn float_to_float(request: &Value) -> Value {
    match text(request, "from") {
        "f32" => f64_bits(f32_arg(request, "bits") as f64),
        "f64" => f32_bits(f64_arg(request, "bits") as f32),
        _ => crate::unknown(request),
    }
}

macro_rules! float_methods {
    ($name:ident, $t:ty, $arg:ident, $bits:ident) => {
        fn $name(request: &Value) -> Value {
            let a: $t = $arg(request, "a");
            let b: $t = $arg(request, "b");
            let c: $t = $arg(request, "c");
            let n = black_box(number_i32(request, "n"));
            match text(request, "f") {
                "max" => $bits(a.max(b)),
                "min" => $bits(a.min(b)),
                "clamp" => $bits(a.clamp(b, c)),
                "signum" => $bits(a.signum()),
                "fract" => $bits(a.fract()),
                "rem" => $bits(a % b),
                "rem_euclid" => $bits(a.rem_euclid(b)),
                "div_euclid" => $bits(a.div_euclid(b)),
                "total_cmp" => json!({ "r": a.total_cmp(&b) as i32 }),
                "powi" => $bits(a.powi(n)),
                "is_nan" => flag(a.is_nan()),
                "is_infinite" => flag(a.is_infinite()),
                "is_finite" => flag(a.is_finite()),
                "is_sign_negative" => flag(a.is_sign_negative()),
                "is_sign_positive" => flag(a.is_sign_positive()),
                "copysign" => $bits(a.copysign(b)),
                "recip" => $bits(a.recip()),
                "to_degrees" => $bits(a.to_degrees()),
                "to_radians" => $bits(a.to_radians()),
                "round" => $bits(a.round()),
                "round_ties_even" => $bits(a.round_ties_even()),
                "floor" => $bits(a.floor()),
                "ceil" => $bits(a.ceil()),
                "trunc" => $bits(a.trunc()),
                "abs" => $bits(a.abs()),
                "sqrt" => $bits(a.sqrt()),
                "mul_add" => $bits(a.mul_add(b, c)),
                "debug" => json!({ "r": format!("{a:?}") }),
                _ => crate::unknown(request),
            }
        }
    };
}

float_methods!(f32_method, f32, f32_arg, f32_bits);
float_methods!(f64_method, f64, f64_arg, f64_bits);

macro_rules! libm_methods {
    ($name:ident, $t:ty, $arg:ident) => {
        fn $name(request: &Value) -> Option<$t> {
            let a: $t = $arg(request, "a");
            let b: $t = $arg(request, "b");
            Some(match text(request, "f") {
                "exp" => a.exp(),
                "exp2" => a.exp2(),
                "exp_m1" => a.exp_m1(),
                "ln" => a.ln(),
                "ln_1p" => a.ln_1p(),
                "log" => a.log(b),
                "log2" => a.log2(),
                "log10" => a.log10(),
                "powf" => a.powf(b),
                "cbrt" => a.cbrt(),
                "hypot" => a.hypot(b),
                "sin" => a.sin(),
                "cos" => a.cos(),
                "tan" => a.tan(),
                "asin" => a.asin(),
                "acos" => a.acos(),
                "atan" => a.atan(),
                "atan2" => a.atan2(b),
                "sinh" => a.sinh(),
                "cosh" => a.cosh(),
                "tanh" => a.tanh(),
                _ => return None,
            })
        }
    };
}

libm_methods!(f32_libm, f32, f32_arg);
libm_methods!(f64_libm, f64, f64_arg);

/// The position of a float among all floats of its type, so that the
/// difference of two positions is their distance in units in the last place.
fn ordinal(bits: u64, sign_bit: u32) -> i128 {
    let magnitude = (bits & ((1u64 << sign_bit) - 1)) as i128;
    if bits >> sign_bit != 0 { -magnitude } else { magnitude }
}

/// Compares a C-library method's result with the Go result in the request.
/// `"libm"` replies only whether they are within the tolerance the request
/// names, which is what the Go tests require; `"libm_ulps"` replies the
/// distance, for measuring.
fn libm(request: &Value, measure: bool) -> Value {
    let (rust_bits, go_bits, sign_bit, rust_nan, go_nan) = match text(request, "ty") {
        "f32" => {
            let Some(result) = f32_libm(request) else { return crate::unknown(request) };
            let go = f32::from_bits(int::<u32>(request, "go"));
            (u64::from(result.to_bits()), u64::from(go.to_bits()), 31, result.is_nan(), go.is_nan())
        }
        "f64" => {
            let Some(result) = f64_libm(request) else { return crate::unknown(request) };
            let go = f64::from_bits(int::<u64>(request, "go"));
            (result.to_bits(), go.to_bits(), 63, result.is_nan(), go.is_nan())
        }
        _ => return crate::unknown(request),
    };
    let ulps = if rust_nan || go_nan {
        if rust_nan && go_nan { 0 } else { u64::MAX }
    } else {
        (ordinal(rust_bits, sign_bit) - ordinal(go_bits, sign_bit)).unsigned_abs().min(u128::from(u64::MAX)) as u64
    };
    if measure {
        return json!({ "ulps": ulps.to_string(), "rust": rust_bits.to_string() });
    }
    let tolerance = request.get("tol").and_then(Value::as_u64).unwrap_or(1);
    if ulps <= tolerance {
        json!({ "close": true })
    } else {
        json!({ "close": false, "ulps": ulps.to_string(), "rust": rust_bits.to_string() })
    }
}

fn wide_text(request: &Value) -> Value {
    macro_rules! formats {
        ($value:expr) => {{
            let value = $value;
            json!({
                "display": format!("{value}"),
                "lower_hex": format!("{value:x}"),
                "upper_hex": format!("{value:X}"),
                "octal": format!("{value:o}"),
                "binary": format!("{value:b}"),
            })
        }};
    }
    match text(request, "ty") {
        "u128" => formats!(int::<u128>(request, "a")),
        "i128" => formats!(int::<i128>(request, "a")),
        _ => crate::unknown(request),
    }
}

fn consts() -> Value {
    json!({
        "f64::NAN": f64::NAN.to_bits().to_string(),
        "f32::NAN": f32::NAN.to_bits().to_string(),
        "f64::EPSILON": f64::EPSILON.to_bits().to_string(),
        "f32::EPSILON": f32::EPSILON.to_bits().to_string(),
        "f64::MIN_POSITIVE": f64::MIN_POSITIVE.to_bits().to_string(),
        "f32::MIN_POSITIVE": f32::MIN_POSITIVE.to_bits().to_string(),
        "f64::MAX": f64::MAX.to_bits().to_string(),
        "f64::MIN": f64::MIN.to_bits().to_string(),
        "f32::MAX": f32::MAX.to_bits().to_string(),
        "f32::MIN": f32::MIN.to_bits().to_string(),
        "f64::INFINITY": f64::INFINITY.to_bits().to_string(),
        "f64::NEG_INFINITY": f64::NEG_INFINITY.to_bits().to_string(),
        "f32::INFINITY": f32::INFINITY.to_bits().to_string(),
        "f32::NEG_INFINITY": f32::NEG_INFINITY.to_bits().to_string(),
        "isize::MAX": isize::MAX.to_string(),
        "isize::MIN": isize::MIN.to_string(),
        "u128::MAX": u128::MAX.to_string(),
        "i128::MAX": i128::MAX.to_string(),
        "i128::MIN": i128::MIN.to_string(),
        "NonZeroUsize::MIN": std::num::NonZeroUsize::MIN.to_string(),
        "f32::consts::PI": std::f32::consts::PI.to_bits().to_string(),
        "f32::consts::TAU": std::f32::consts::TAU.to_bits().to_string(),
        "f32::consts::FRAC_PI_2": std::f32::consts::FRAC_PI_2.to_bits().to_string(),
        "f64::consts::PI": std::f64::consts::PI.to_bits().to_string(),
        "f64::consts::TAU": std::f64::consts::TAU.to_bits().to_string(),
    })
}

pub fn handle(request: &Value) -> Value {
    // Many of the cases panic on purpose; the reply carries the message, so
    // the default hook's report on standard error is only noise.
    static QUIET: std::sync::Once = std::sync::Once::new();
    QUIET.call_once(|| std::panic::set_hook(Box::new(|_| {})));
    match text(request, "op") {
        "int" => int_method(request),
        "usize63" => usize63(request),
        "try_from" => try_from(request),
        "from_str_radix" => from_str_radix(request),
        "bytes" => bytes(request),
        "float_to_int" => float_to_int(request),
        "int_to_float" => int_to_float(request),
        "float_to_float" => float_to_float(request),
        "float" => match text(request, "ty") {
            "f32" => f32_method(request),
            "f64" => f64_method(request),
            _ => crate::unknown(request),
        },
        "libm" => libm(request, false),
        "libm_ulps" => libm(request, true),
        "wide_text" => wide_text(request),
        "consts" => consts(),
        _ => crate::unknown(request),
    }
}
