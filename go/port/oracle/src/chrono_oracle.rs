//! Oracle module `chrono`: chrono 0.4.44 and chrono-tz 0.10.4.
//!
//! Requests are objects with an `"op"` member. The Go tests that use this
//! module are `go/crates/chrono/oracle_test.go` and
//! `go/crates/chronotz/oracle_test.go`; each operation is described where the
//! Go side builds its requests.
//!
//! Conventions:
//!
//! - A zone is `"utc"`, `"fixed:<seconds east>"`, `"tz:<IANA name>"` or
//!   `"local"`.
//! - A request with a `"tzenv"` member runs on a fresh thread after setting
//!   (string) or removing (null) the `TZ` variable, because chrono caches the
//!   local zone per thread. A value that starts with `@` names a file relative
//!   to the working directory and is made absolute, so a recorded corpus does
//!   not hold one machine's paths; `@:` does the same behind a leading colon.
//! - A value that can panic is wrapped by `catch`, and is then
//!   `{"panic": "<message>"}`.

use std::fmt::Display;
use std::panic::{self, AssertUnwindSafe};
use std::str::FromStr;
use std::sync::Once;

use chrono::format::{Item, Parsed, StrftimeItems};
use chrono::{
    DateTime, Datelike, Days, DurationRound, FixedOffset, Local, MappedLocalTime, Month, Months,
    NaiveDate, NaiveDateTime, NaiveTime, Offset, SecondsFormat, SubsecRound, TimeDelta, TimeZone,
    Timelike, Utc, Weekday,
};
use chrono_tz::{OffsetComponents, OffsetName, Tz};
use serde_json::{Value, json};

/// Panics are answers here, so the default hook's report would only be noise.
fn quiet() {
    static ONCE: Once = Once::new();
    ONCE.call_once(|| panic::set_hook(Box::new(|_| {})));
}

fn panic_message(payload: Box<dyn std::any::Any + Send>) -> String {
    payload
        .downcast_ref::<String>()
        .cloned()
        .or_else(|| payload.downcast_ref::<&str>().map(|text| (*text).to_owned()))
        .unwrap_or_else(|| "non-string panic payload".to_owned())
}

fn catch(f: impl FnOnce() -> Value) -> Value {
    match panic::catch_unwind(AssertUnwindSafe(f)) {
        Ok(value) => value,
        Err(payload) => json!({ "panic": panic_message(payload) }),
    }
}

fn int(request: &Value, key: &str) -> i64 {
    request.get(key).and_then(Value::as_i64).unwrap_or_default()
}

fn uint(request: &Value, key: &str) -> u64 {
    request.get(key).and_then(Value::as_u64).unwrap_or_default()
}

fn text<'a>(request: &'a Value, key: &str) -> &'a str {
    crate::text(request, key)
}

#[derive(Clone, Copy)]
enum Zone {
    Utc,
    Fixed(FixedOffset),
    Tz(Tz),
    Local,
}

fn zone(request: &Value, key: &str) -> Option<Zone> {
    let spec = text(request, key);
    if spec == "utc" || spec.is_empty() {
        Some(Zone::Utc)
    } else if spec == "local" {
        Some(Zone::Local)
    } else if let Some(secs) = spec.strip_prefix("fixed:") {
        FixedOffset::east_opt(secs.parse().ok()?).map(Zone::Fixed)
    } else if let Some(name) = spec.strip_prefix("tz:") {
        name.parse().ok().map(Zone::Tz)
    } else {
        None
    }
}

/// Runs `$body` with `$tz` bound to the zone's own type.
macro_rules! zoned {
    ($zone:expr, |$tz:ident| $body:expr) => {
        match $zone {
            Zone::Utc => {
                let $tz = Utc;
                $body
            }
            Zone::Fixed(offset) => {
                let $tz = offset;
                $body
            }
            Zone::Tz(tz) => {
                let $tz = tz;
                $body
            }
            Zone::Local => {
                let $tz = Local;
                $body
            }
        }
    };
}

fn utc_instant(request: &Value) -> Option<DateTime<Utc>> {
    DateTime::from_timestamp(int(request, "secs"), uint(request, "nanos") as u32)
}

fn describe_dt<Tz: TimeZone>(dt: &DateTime<Tz>) -> Value {
    json!([
        format!("{dt:?}"),
        dt.timestamp(),
        dt.timestamp_subsec_nanos(),
        dt.offset().fix().local_minus_utc()
    ])
}

fn describe_opt_dt<Tz: TimeZone>(dt: Option<DateTime<Tz>>) -> Value {
    dt.map_or(Value::Null, |dt| describe_dt(&dt))
}

fn describe_local_result<Tz: TimeZone>(result: MappedLocalTime<DateTime<Tz>>) -> Value {
    match result {
        MappedLocalTime::None => json!({ "n": true }),
        MappedLocalTime::Single(dt) => json!({ "s": describe_dt(&dt) }),
        MappedLocalTime::Ambiguous(a, b) => json!({ "a": [describe_dt(&a), describe_dt(&b)] }),
    }
}

fn describe_delta(delta: TimeDelta) -> Value {
    let std = match delta.to_std() {
        Ok(std) => json!([std.as_secs(), std.subsec_nanos()]),
        Err(error) => json!(error.to_string()),
    };
    json!([
        delta.to_string(),
        format!("{delta:?}"),
        delta.num_weeks(),
        delta.num_days(),
        delta.num_hours(),
        delta.num_minutes(),
        delta.num_seconds(),
        delta.num_milliseconds(),
        delta.num_microseconds(),
        delta.num_nanoseconds(),
        delta.subsec_millis(),
        delta.subsec_micros(),
        delta.subsec_nanos(),
        delta.is_zero(),
        delta.abs().to_string(),
        (-delta).to_string(),
        std
    ])
}

fn describe_opt_delta(delta: Option<TimeDelta>) -> Value {
    delta.map_or(Value::Null, describe_delta)
}

fn delta_arg(value: Option<&Value>) -> Option<TimeDelta> {
    let pair = value?.as_array()?;
    TimeDelta::new(pair.first()?.as_i64()?, pair.get(1)?.as_u64()? as u32)
}

fn describe_date(date: NaiveDate) -> Value {
    json!([
        format!("{date:?}"),
        date.to_string(),
        date.year(),
        date.month(),
        date.day(),
        date.ordinal(),
        date.weekday().num_days_from_monday(),
        date.iso_week().year(),
        date.iso_week().week(),
        date.num_days_from_ce(),
        date.to_epoch_days(),
        date.leap_year(),
        date.quarter(),
        date.num_days_in_month(),
        format!("{:?}", date.iso_week())
    ])
}

fn describe_opt_date(date: Option<NaiveDate>) -> Value {
    date.map_or(Value::Null, describe_date)
}

fn describe_time(time: NaiveTime) -> Value {
    json!([
        format!("{time:?}"),
        time.to_string(),
        time.hour(),
        time.minute(),
        time.second(),
        time.nanosecond(),
        time.num_seconds_from_midnight(),
        time.hour12().0,
        time.hour12().1
    ])
}

fn describe_opt_time(time: Option<NaiveTime>) -> Value {
    time.map_or(Value::Null, describe_time)
}

fn describe_naive(naive: NaiveDateTime) -> Value {
    json!([format!("{naive:?}"), naive.to_string()])
}

fn describe_opt_naive(naive: Option<NaiveDateTime>) -> Value {
    naive.map_or(Value::Null, describe_naive)
}

fn weekday(number: i64) -> Weekday {
    Weekday::try_from(number.rem_euclid(7) as u8).expect("weekday")
}

/// A date from its day count, which reaches every date chrono can hold.
fn date_arg(request: &Value, key: &str) -> Option<NaiveDate> {
    NaiveDate::from_num_days_from_ce_opt(i32::try_from(int(request, key)).ok()?)
}

fn time_arg(request: &Value, secs: &str, frac: &str) -> Option<NaiveTime> {
    NaiveTime::from_num_seconds_from_midnight_opt(
        u32::try_from(uint(request, secs)).ok()?,
        u32::try_from(uint(request, frac)).ok()?,
    )
}

fn naive_arg(request: &Value) -> Option<NaiveDateTime> {
    Some(date_arg(request, "days")?.and_time(time_arg(request, "tsecs", "frac")?))
}

const SECONDS_FORMATS: [SecondsFormat; 5] = [
    SecondsFormat::Secs,
    SecondsFormat::Millis,
    SecondsFormat::Micros,
    SecondsFormat::Nanos,
    SecondsFormat::AutoSi,
];

fn text_forms<Tz: TimeZone>(dt: &DateTime<Tz>) -> Value
where
    Tz::Offset: Display,
{
    let opts = |use_z: bool| -> Vec<Value> {
        SECONDS_FORMATS
            .iter()
            .map(|secform| catch(|| json!(dt.to_rfc3339_opts(*secform, use_z))))
            .collect()
    };
    json!({
        "d": dt.to_string(),
        "g": format!("{dt:?}"),
        "r": dt.to_rfc3339(),
        "s": opts(false),
        "zs": opts(true),
        "m": catch(|| json!(dt.to_rfc2822())),
        "j": catch(|| match serde_json::to_string(dt) {
            Ok(encoded) => json!(encoded),
            Err(error) => json!({ "err": error.to_string() }),
        }),
        "o": dt.offset().to_string(),
        "og": format!("{:?}", dt.offset()),
    })
}

fn fields<Tz: TimeZone>(dt: &DateTime<Tz>) -> Value {
    json!([
        dt.year(),
        dt.month(),
        dt.day(),
        dt.hour(),
        dt.minute(),
        dt.second(),
        dt.nanosecond(),
        dt.ordinal(),
        dt.weekday().num_days_from_monday(),
        dt.iso_week().year(),
        dt.iso_week().week(),
        dt.quarter(),
        dt.hour12().0,
        dt.hour12().1,
        dt.num_days_from_ce(),
        dt.timestamp(),
        dt.timestamp_millis(),
        dt.timestamp_micros(),
        dt.timestamp_nanos_opt(),
        dt.timestamp_subsec_millis(),
        dt.timestamp_subsec_micros(),
        dt.timestamp_subsec_nanos(),
        dt.offset().fix().local_minus_utc(),
        dt.num_seconds_from_midnight(),
        dt.year_ce().0,
        dt.year_ce().1,
        dt.month0(),
        dt.day0(),
        dt.ordinal0(),
        format!("{:?}", dt.naive_utc()),
        catch(|| json!(format!("{:?}", dt.naive_local()))),
        catch(|| json!(format!("{:?}", dt.date_naive()))),
        format!("{:?}", dt.time()),
        format!("{:?}", dt.fixed_offset()),
        format!("{:?}", dt.to_utc()),
        format!("{}", dt.weekday()),
        format!("{:?}", dt.weekday()),
        dt.weekday().number_from_monday(),
        dt.weekday().number_from_sunday(),
        dt.weekday().num_days_from_sunday(),
        format!("{:?}", dt.weekday().succ()),
        format!("{:?}", dt.weekday().pred())
    ])
}

fn describe_item(item: &Item<'_>) -> Value {
    match item {
        Item::Literal(text) => json!(["L", text]),
        Item::OwnedLiteral(text) => json!(["OL", text]),
        Item::Space(text) => json!(["S", text]),
        Item::OwnedSpace(text) => json!(["OS", text]),
        Item::Numeric(numeric, pad) => json!(["N", format!("{numeric:?}"), format!("{pad:?}")]),
        Item::Fixed(fixed) => json!(["F", format!("{fixed:?}")]),
        Item::Error => json!(["E"]),
    }
}

fn option_json<T: Into<Value>>(value: Option<T>) -> Value {
    value.map_or(Value::Null, Into::into)
}

fn parse_error(error: chrono::ParseError) -> Value {
    json!({ "err": error.to_string(), "kind": format!("{:?}", error.kind()), "dbg": format!("{error:?}") })
}

fn parse_result<T>(result: Result<T, chrono::ParseError>, describe: impl FnOnce(T) -> Value) -> Value {
    match result {
        Ok(value) => json!({ "ok": describe(value) }),
        Err(error) => parse_error(error),
    }
}

fn describe_parsed(parsed: &Parsed) -> Value {
    json!({
        "f": [
            option_json(parsed.year()),
            option_json(parsed.year_div_100()),
            option_json(parsed.year_mod_100()),
            option_json(parsed.isoyear()),
            option_json(parsed.isoyear_div_100()),
            option_json(parsed.isoyear_mod_100()),
            option_json(parsed.quarter()),
            option_json(parsed.month()),
            option_json(parsed.week_from_sun()),
            option_json(parsed.week_from_mon()),
            option_json(parsed.isoweek()),
            option_json(parsed.weekday().map(|day| day.num_days_from_monday())),
            option_json(parsed.ordinal()),
            option_json(parsed.day()),
            option_json(parsed.hour_div_12()),
            option_json(parsed.hour_mod_12()),
            option_json(parsed.minute()),
            option_json(parsed.second()),
            option_json(parsed.nanosecond()),
            option_json(parsed.timestamp()),
            option_json(parsed.offset())
        ],
        "date": parse_result(parsed.to_naive_date(), |date| json!(format!("{date:?}"))),
        "time": parse_result(parsed.to_naive_time(), |time| json!(format!("{time:?}"))),
        "naive": parse_result(parsed.to_naive_datetime_with_offset(0), |naive| json!(format!("{naive:?}"))),
        "naive3600": parse_result(parsed.to_naive_datetime_with_offset(3600), |naive| json!(format!("{naive:?}"))),
        "fixed": parse_result(parsed.to_fixed_offset(), |offset| json!(offset.local_minus_utc())),
        "dt": parse_result(parsed.to_datetime(), |dt| describe_dt(&dt)),
        "utc": parse_result(parsed.to_datetime_with_timezone(&Utc), |dt| describe_dt(&dt)),
        "plus9": parse_result(
            parsed.to_datetime_with_timezone(&FixedOffset::east_opt(9 * 3600).unwrap()),
            |dt| describe_dt(&dt),
        ),
    })
}

fn parse(request: &Value) -> Value {
    let input = text(request, "s");
    let format = text(request, "f");
    let with_rest = |value: Value, rest: &str| json!([value, rest]);
    match text(request, "fn") {
        "rfc3339" => parse_result(DateTime::parse_from_rfc3339(input), |dt| describe_dt(&dt)),
        "rfc2822" => parse_result(DateTime::parse_from_rfc2822(input), |dt| describe_dt(&dt)),
        "dt" => parse_result(DateTime::parse_from_str(input, format), |dt| describe_dt(&dt)),
        "dt_rem" => parse_result(DateTime::parse_and_remainder(input, format), |(dt, rest)| {
            with_rest(describe_dt(&dt), rest)
        }),
        "dt_fixed_fromstr" => {
            parse_result(input.parse::<DateTime<FixedOffset>>(), |dt| describe_dt(&dt))
        }
        "dt_utc_fromstr" => parse_result(input.parse::<DateTime<Utc>>(), |dt| describe_dt(&dt)),
        "dt_local_fromstr" => parse_result(input.parse::<DateTime<Local>>(), |dt| describe_dt(&dt)),
        "ndt" => parse_result(NaiveDateTime::parse_from_str(input, format), describe_naive),
        "ndt_rem" => parse_result(NaiveDateTime::parse_and_remainder(input, format), |(naive, rest)| {
            with_rest(describe_naive(naive), rest)
        }),
        "ndt_fromstr" => parse_result(input.parse::<NaiveDateTime>(), describe_naive),
        "nd" => parse_result(NaiveDate::parse_from_str(input, format), describe_date),
        "nd_rem" => parse_result(NaiveDate::parse_and_remainder(input, format), |(date, rest)| {
            with_rest(describe_date(date), rest)
        }),
        "nd_fromstr" => parse_result(input.parse::<NaiveDate>(), describe_date),
        "nt" => parse_result(NaiveTime::parse_from_str(input, format), describe_time),
        "nt_rem" => parse_result(NaiveTime::parse_and_remainder(input, format), |(time, rest)| {
            with_rest(describe_time(time), rest)
        }),
        "nt_fromstr" => parse_result(input.parse::<NaiveTime>(), describe_time),
        "fixed_fromstr" => parse_result(FixedOffset::from_str(input), |offset| {
            json!([offset.local_minus_utc(), offset.to_string()])
        }),
        "weekday_fromstr" => match input.parse::<Weekday>() {
            Ok(day) => json!({ "ok": format!("{day:?}") }),
            Err(error) => json!({ "err": error.to_string(), "dbg": format!("{error:?}") }),
        },
        "month_fromstr" => match input.parse::<Month>() {
            Ok(month) => json!({ "ok": format!("{month:?}") }),
            Err(error) => json!({ "err": error.to_string(), "dbg": format!("{error:?}") }),
        },
        "parsed" => {
            let mut parsed = Parsed::new();
            match chrono::format::parse(&mut parsed, input, StrftimeItems::new(format)) {
                Ok(()) => json!({ "ok": describe_parsed(&parsed) }),
                Err(error) => parse_error(error),
            }
        }
        "parsed_rem" => {
            let mut parsed = Parsed::new();
            match chrono::format::parse_and_remainder(&mut parsed, input, StrftimeItems::new(format)) {
                Ok(rest) => json!({ "ok": [describe_parsed(&parsed), rest] }),
                Err(error) => parse_error(error),
            }
        }
        _ => crate::unknown(request),
    }
}

fn format(request: &Value) -> Value {
    let spec = text(request, "f");
    let lenient = request.get("lenient").and_then(Value::as_bool).unwrap_or(false);
    let Some(utc) = utc_instant(request) else {
        return Value::Null;
    };
    let Some(zone) = zone(request, "z") else {
        return crate::unknown(request);
    };
    let items = || if lenient { StrftimeItems::new_lenient(spec) } else { StrftimeItems::new(spec) };
    match text(request, "k") {
        "dt" | "" => zoned!(zone, |tz| {
            let dt = utc.with_timezone(&tz);
            catch(|| json!(dt.format_with_items(items()).to_string()))
        }),
        "ndt" => catch(|| json!(utc.naive_utc().format_with_items(items()).to_string())),
        "nd" => catch(|| json!(utc.naive_utc().date().format_with_items(items()).to_string())),
        "nt" => catch(|| json!(utc.naive_utc().time().format_with_items(items()).to_string())),
        _ => crate::unknown(request),
    }
}

fn delta(request: &Value) -> Value {
    let v = int(request, "v");
    let a = delta_arg(request.get("a"));
    let b = delta_arg(request.get("b"));
    let n = int(request, "n") as i32;
    let both = |f: &dyn Fn(TimeDelta, TimeDelta) -> Value| match (a, b) {
        (Some(a), Some(b)) => catch(|| f(a, b)),
        _ => Value::Null,
    };
    let one = |f: &dyn Fn(TimeDelta) -> Value| match a {
        Some(a) => catch(|| f(a)),
        None => Value::Null,
    };
    match text(request, "c") {
        "new" => describe_opt_delta(TimeDelta::new(v, uint(request, "nanos") as u32)),
        "weeks" => catch(|| describe_delta(TimeDelta::weeks(v))),
        "days" => catch(|| describe_delta(TimeDelta::days(v))),
        "hours" => catch(|| describe_delta(TimeDelta::hours(v))),
        "minutes" => catch(|| describe_delta(TimeDelta::minutes(v))),
        "seconds" => catch(|| describe_delta(TimeDelta::seconds(v))),
        "milliseconds" => catch(|| describe_delta(TimeDelta::milliseconds(v))),
        "microseconds" => catch(|| describe_delta(TimeDelta::microseconds(v))),
        "nanoseconds" => catch(|| describe_delta(TimeDelta::nanoseconds(v))),
        "try_weeks" => describe_opt_delta(TimeDelta::try_weeks(v)),
        "try_days" => describe_opt_delta(TimeDelta::try_days(v)),
        "try_hours" => describe_opt_delta(TimeDelta::try_hours(v)),
        "try_minutes" => describe_opt_delta(TimeDelta::try_minutes(v)),
        "try_seconds" => describe_opt_delta(TimeDelta::try_seconds(v)),
        "try_milliseconds" => describe_opt_delta(TimeDelta::try_milliseconds(v)),
        "min" => describe_delta(TimeDelta::MIN),
        "max" => describe_delta(TimeDelta::MAX),
        "zero" => describe_delta(TimeDelta::zero()),
        "from_std" => {
            match TimeDelta::from_std(std::time::Duration::new(uint(request, "v"), uint(request, "nanos") as u32)) {
                Ok(delta) => json!({ "ok": describe_delta(delta) }),
                Err(error) => json!({ "err": error.to_string(), "dbg": format!("{error:?}") }),
            }
        }
        "add" => both(&|a, b| describe_delta(a + b)),
        "sub" => both(&|a, b| describe_delta(a - b)),
        "checked_add" => both(&|a, b| describe_opt_delta(a.checked_add(&b))),
        "checked_sub" => both(&|a, b| describe_opt_delta(a.checked_sub(&b))),
        "cmp" => both(&|a, b| json!([a.cmp(&b) as i8, a == b])),
        "mul" => one(&|a| describe_delta(a * n)),
        "div" => one(&|a| describe_delta(a / n)),
        "checked_mul" => one(&|a| describe_opt_delta(a.checked_mul(n))),
        "checked_div" => one(&|a| describe_opt_delta(a.checked_div(n))),
        "describe" => one(&describe_delta),
        "floats" => one(&|a| json!([a.as_seconds_f64(), a.as_seconds_f32() as f64])),
        _ => crate::unknown(request),
    }
}

fn arith_on<Tz: TimeZone>(request: &Value, dt: DateTime<Tz>) -> Value
where
    Tz::Offset: Display + Copy,
{
    let v = int(request, "v");
    let d = delta_arg(request.get("d"));
    let other = || DateTime::from_timestamp(int(request, "osecs"), uint(request, "onanos") as u32);
    let with_delta = |f: &dyn Fn(TimeDelta) -> Value| match d {
        Some(d) => catch(|| f(d)),
        None => Value::Null,
    };
    let rounding = |result: Result<DateTime<Tz>, chrono::RoundingError>| match result {
        Ok(dt) => json!({ "ok": describe_dt(&dt) }),
        Err(error) => json!({ "err": error.to_string(), "dbg": format!("{error:?}") }),
    };
    match text(request, "a") {
        "checked_add_signed" => with_delta(&|d| describe_opt_dt(dt.checked_add_signed(d))),
        "checked_sub_signed" => with_delta(&|d| describe_opt_dt(dt.checked_sub_signed(d))),
        "add" => with_delta(&|d| describe_dt(&(dt + d))),
        "sub" => with_delta(&|d| describe_dt(&(dt - d))),
        "checked_add_months" => describe_opt_dt(dt.checked_add_months(Months::new(v as u32))),
        "checked_sub_months" => describe_opt_dt(dt.checked_sub_months(Months::new(v as u32))),
        "add_months" => catch(|| describe_dt(&(dt + Months::new(v as u32)))),
        "sub_months" => catch(|| describe_dt(&(dt - Months::new(v as u32)))),
        "checked_add_days" => describe_opt_dt(dt.checked_add_days(Days::new(uint(request, "v")))),
        "checked_sub_days" => describe_opt_dt(dt.checked_sub_days(Days::new(uint(request, "v")))),
        "add_days" => catch(|| describe_dt(&(dt + Days::new(uint(request, "v"))))),
        "sub_days" => catch(|| describe_dt(&(dt - Days::new(uint(request, "v"))))),
        "since" => match other() {
            Some(other) => json!([
                describe_delta(dt.signed_duration_since(other)),
                dt.cmp(&other.with_timezone(&dt.timezone())) as i8,
                dt == other,
                option_json(dt.years_since(other.with_timezone(&dt.timezone())))
            ]),
            None => Value::Null,
        },
        "with_year" => describe_opt_dt(dt.with_year(v as i32)),
        "with_month" => describe_opt_dt(dt.with_month(v as u32)),
        "with_month0" => describe_opt_dt(dt.with_month0(v as u32)),
        "with_day" => describe_opt_dt(dt.with_day(v as u32)),
        "with_day0" => describe_opt_dt(dt.with_day0(v as u32)),
        "with_ordinal" => describe_opt_dt(dt.with_ordinal(v as u32)),
        "with_ordinal0" => describe_opt_dt(dt.with_ordinal0(v as u32)),
        "with_hour" => describe_opt_dt(dt.with_hour(v as u32)),
        "with_minute" => describe_opt_dt(dt.with_minute(v as u32)),
        "with_second" => describe_opt_dt(dt.with_second(v as u32)),
        "with_nanosecond" => describe_opt_dt(dt.with_nanosecond(v as u32)),
        "with_time" => match time_arg(request, "tsecs", "frac") {
            Some(time) => describe_local_result(dt.with_time(time)),
            None => Value::Null,
        },
        "add_fixed" => match FixedOffset::east_opt(v as i32) {
            Some(offset) => catch(|| describe_dt(&(dt + offset))),
            None => Value::Null,
        },
        "sub_fixed" => match FixedOffset::east_opt(v as i32) {
            Some(offset) => catch(|| describe_dt(&(dt - offset))),
            None => Value::Null,
        },
        "add_std" => catch(|| {
            describe_dt(&(dt + std::time::Duration::new(uint(request, "v"), uint(request, "nanos") as u32)))
        }),
        "sub_std" => catch(|| {
            describe_dt(&(dt - std::time::Duration::new(uint(request, "v"), uint(request, "nanos") as u32)))
        }),
        "round_subsecs" => catch(|| describe_dt(&dt.round_subsecs(v as u16))),
        "trunc_subsecs" => catch(|| describe_dt(&dt.trunc_subsecs(v as u16))),
        "duration_round" => with_delta(&|d| rounding(dt.duration_round(d))),
        "duration_trunc" => with_delta(&|d| rounding(dt.duration_trunc(d))),
        "duration_round_up" => with_delta(&|d| rounding(dt.duration_round_up(d))),
        "system_time" => {
            let system: std::time::SystemTime = dt.into();
            let back = DateTime::<Utc>::from(system);
            describe_dt(&back)
        }
        _ => crate::unknown(request),
    }
}

fn arith(request: &Value) -> Value {
    let Some(utc) = utc_instant(request) else {
        return Value::Null;
    };
    let Some(zone) = zone(request, "z") else {
        return crate::unknown(request);
    };
    zoned!(zone, |tz| arith_on(request, utc.with_timezone(&tz)))
}

fn instant(request: &Value, which: &str) -> Value {
    let Some(utc) = utc_instant(request) else {
        return Value::Null;
    };
    let Some(zone) = zone(request, "z") else {
        return crate::unknown(request);
    };
    zoned!(zone, |tz| {
        let dt = utc.with_timezone(&tz);
        if which == "text" { text_forms(&dt) } else { fields(&dt) }
    })
}

fn zone_op(request: &Value) -> Value {
    let Some(zone) = zone(request, "z") else {
        return crate::unknown(request);
    };
    let v = int(request, "v");
    zoned!(zone, |tz| match text(request, "a") {
        "ymdhms" => describe_local_result(tz.with_ymd_and_hms(
            int(request, "y") as i32,
            uint(request, "mo") as u32,
            uint(request, "d") as u32,
            uint(request, "h") as u32,
            uint(request, "mi") as u32,
            uint(request, "s") as u32,
        )),
        "ts" => describe_local_result(tz.timestamp_opt(v, uint(request, "nanos") as u32)),
        "ts_millis" => describe_local_result(tz.timestamp_millis_opt(v)),
        "ts_micros" => describe_local_result(tz.timestamp_micros(v)),
        "ts_nanos" => describe_dt(&tz.timestamp_nanos(v)),
        "from_local" => match naive_arg(request) {
            Some(naive) => describe_local_result(tz.from_local_datetime(&naive)),
            None => Value::Null,
        },
        "from_utc" => match naive_arg(request) {
            Some(naive) => describe_dt(&tz.from_utc_datetime(&naive)),
            None => Value::Null,
        },
        "and_local" => match naive_arg(request) {
            Some(naive) => describe_local_result(naive.and_local_timezone(tz)),
            None => Value::Null,
        },
        "offset_local_date" => match date_arg(request, "days") {
            Some(date) => match tz.offset_from_local_date(&date) {
                MappedLocalTime::None => json!({ "n": true }),
                MappedLocalTime::Single(offset) => json!({ "s": offset.to_string() }),
                MappedLocalTime::Ambiguous(a, b) => json!({ "a": [a.to_string(), b.to_string()] }),
            },
            None => Value::Null,
        },
        "offset_utc_date" => match date_arg(request, "days") {
            Some(date) => json!(tz.offset_from_utc_date(&date).to_string()),
            None => Value::Null,
        },
        "unwrap_local" => match naive_arg(request) {
            Some(naive) => catch(|| describe_dt(&tz.from_local_datetime(&naive).unwrap())),
            None => Value::Null,
        },
        _ => crate::unknown(request),
    })
}

fn naive(request: &Value) -> Value {
    let v = int(request, "v");
    let y = int(request, "y") as i32;
    let d = delta_arg(request.get("d"));
    let date = date_arg(request, "days");
    let time = time_arg(request, "tsecs", "frac");
    let datetime = naive_arg(request);
    let other_date = date_arg(request, "odays");
    let other_time = time_arg(request, "otsecs", "ofrac");
    let on_date = |f: &dyn Fn(NaiveDate) -> Value| date.map_or(Value::Null, |date| catch(|| f(date)));
    let on_time = |f: &dyn Fn(NaiveTime) -> Value| time.map_or(Value::Null, |time| catch(|| f(time)));
    let on_naive =
        |f: &dyn Fn(NaiveDateTime) -> Value| datetime.map_or(Value::Null, |naive| catch(|| f(naive)));
    match text(request, "a") {
        // Constructors.
        "ymd" => describe_opt_date(NaiveDate::from_ymd_opt(y, uint(request, "mo") as u32, uint(request, "d") as u32)),
        "yo" => describe_opt_date(NaiveDate::from_yo_opt(y, uint(request, "o") as u32)),
        "isoywd" => describe_opt_date(NaiveDate::from_isoywd_opt(y, uint(request, "w") as u32, weekday(v))),
        "ce" => describe_opt_date(NaiveDate::from_num_days_from_ce_opt(v as i32)),
        "epoch_days" => describe_opt_date(NaiveDate::from_epoch_days(v as i32)),
        "wom" => describe_opt_date(NaiveDate::from_weekday_of_month_opt(
            y,
            uint(request, "mo") as u32,
            weekday(v),
            uint(request, "n") as u8,
        )),
        "hms_nano" => describe_opt_time(NaiveTime::from_hms_nano_opt(
            uint(request, "h") as u32,
            uint(request, "mi") as u32,
            uint(request, "s") as u32,
            uint(request, "n") as u32,
        )),
        "hms_milli" => describe_opt_time(NaiveTime::from_hms_milli_opt(
            uint(request, "h") as u32,
            uint(request, "mi") as u32,
            uint(request, "s") as u32,
            uint(request, "n") as u32,
        )),
        "hms_micro" => describe_opt_time(NaiveTime::from_hms_micro_opt(
            uint(request, "h") as u32,
            uint(request, "mi") as u32,
            uint(request, "s") as u32,
            uint(request, "n") as u32,
        )),
        "secs_nano" => describe_opt_time(NaiveTime::from_num_seconds_from_midnight_opt(
            uint(request, "tsecs") as u32,
            uint(request, "frac") as u32,
        )),
        "limits" => json!([
            describe_date(NaiveDate::MIN),
            describe_date(NaiveDate::MAX),
            describe_naive(NaiveDateTime::MIN),
            describe_naive(NaiveDateTime::MAX),
            describe_dt(&DateTime::<Utc>::MIN_UTC),
            describe_dt(&DateTime::<Utc>::MAX_UTC),
            describe_dt(&DateTime::UNIX_EPOCH),
            describe_time(NaiveTime::MIN),
            describe_dt(&DateTime::<Utc>::default()),
            describe_dt(&DateTime::<FixedOffset>::default()),
            describe_date(NaiveDate::default()),
            describe_time(NaiveTime::default()),
            describe_naive(NaiveDateTime::default()),
            describe_delta(TimeDelta::default())
        ]),
        // NaiveDate.
        "date" => on_date(&describe_date),
        "date_add_days" => on_date(&|date| describe_opt_date(date.checked_add_days(Days::new(uint(request, "v"))))),
        "date_sub_days" => on_date(&|date| describe_opt_date(date.checked_sub_days(Days::new(uint(request, "v"))))),
        "date_plus_days" => on_date(&|date| describe_date(date + Days::new(uint(request, "v")))),
        "date_minus_days" => on_date(&|date| describe_date(date - Days::new(uint(request, "v")))),
        "date_add_months" => on_date(&|date| describe_opt_date(date.checked_add_months(Months::new(v as u32)))),
        "date_sub_months" => on_date(&|date| describe_opt_date(date.checked_sub_months(Months::new(v as u32)))),
        "date_plus_months" => on_date(&|date| describe_date(date + Months::new(v as u32))),
        "date_minus_months" => on_date(&|date| describe_date(date - Months::new(v as u32))),
        "date_add_signed" => on_date(&|date| d.map_or(Value::Null, |d| describe_opt_date(date.checked_add_signed(d)))),
        "date_sub_signed" => on_date(&|date| d.map_or(Value::Null, |d| describe_opt_date(date.checked_sub_signed(d)))),
        "date_plus" => on_date(&|date| d.map_or(Value::Null, |d| describe_date(date + d))),
        "date_minus" => on_date(&|date| d.map_or(Value::Null, |d| describe_date(date - d))),
        "date_since" => on_date(&|date| {
            other_date.map_or(Value::Null, |other| {
                json!([
                    describe_delta(date.signed_duration_since(other)),
                    option_json(date.years_since(other)),
                    date.abs_diff(other) == Days::new(uint(request, "v")),
                    date.cmp(&other) as i8
                ])
            })
        }),
        "date_succ" => on_date(&|date| describe_opt_date(date.succ_opt())),
        "date_pred" => on_date(&|date| describe_opt_date(date.pred_opt())),
        "date_with_year" => on_date(&|date| describe_opt_date(date.with_year(v as i32))),
        "date_with_month" => on_date(&|date| describe_opt_date(date.with_month(v as u32))),
        "date_with_month0" => on_date(&|date| describe_opt_date(date.with_month0(v as u32))),
        "date_with_day" => on_date(&|date| describe_opt_date(date.with_day(v as u32))),
        "date_with_day0" => on_date(&|date| describe_opt_date(date.with_day0(v as u32))),
        "date_with_ordinal" => on_date(&|date| describe_opt_date(date.with_ordinal(v as u32))),
        "date_with_ordinal0" => on_date(&|date| describe_opt_date(date.with_ordinal0(v as u32))),
        "date_week" => on_date(&|date| {
            let week = date.week(weekday(v));
            json!([
                describe_opt_date(week.checked_first_day()),
                describe_opt_date(week.checked_last_day()),
                catch(|| describe_date(week.first_day())),
                catch(|| describe_date(week.last_day()))
            ])
        }),
        "date_iter_days" => on_date(&|date| {
            let n = uint(request, "n") as usize;
            json!([
                date.iter_days().take(n).map(|day| day.to_string()).collect::<Vec<_>>(),
                date.iter_days().rev().take(n).map(|day| day.to_string()).collect::<Vec<_>>(),
                date.iter_weeks().take(n).map(|day| day.to_string()).collect::<Vec<_>>(),
                date.iter_weeks().rev().take(n).map(|day| day.to_string()).collect::<Vec<_>>()
            ])
        }),
        "date_and_hms" => on_date(&|date| {
            json!([
                describe_opt_naive(date.and_hms_opt(uint(request, "h") as u32, uint(request, "mi") as u32, uint(request, "s") as u32)),
                describe_opt_naive(date.and_hms_milli_opt(uint(request, "h") as u32, uint(request, "mi") as u32, uint(request, "s") as u32, uint(request, "n") as u32)),
                describe_opt_naive(date.and_hms_micro_opt(uint(request, "h") as u32, uint(request, "mi") as u32, uint(request, "s") as u32, uint(request, "n") as u32)),
                describe_opt_naive(date.and_hms_nano_opt(uint(request, "h") as u32, uint(request, "mi") as u32, uint(request, "s") as u32, uint(request, "n") as u32))
            ])
        }),
        // NaiveTime.
        "time" => on_time(&describe_time),
        "time_add" => on_time(&|time| {
            d.map_or(Value::Null, |d| {
                let (sum, wrap) = time.overflowing_add_signed(d);
                let (difference, unwrap) = time.overflowing_sub_signed(d);
                json!([describe_time(sum), wrap, describe_time(difference), unwrap, describe_time(time + d), describe_time(time - d)])
            })
        }),
        "time_since" => on_time(&|time| {
            other_time.map_or(Value::Null, |other| {
                json!([describe_delta(time.signed_duration_since(other)), time.cmp(&other) as i8])
            })
        }),
        "time_with" => on_time(&|time| {
            json!([
                describe_opt_time(time.with_hour(v as u32)),
                describe_opt_time(time.with_minute(v as u32)),
                describe_opt_time(time.with_second(v as u32)),
                describe_opt_time(time.with_nanosecond(v as u32))
            ])
        }),
        "time_add_std" => on_time(&|time| {
            let std = std::time::Duration::new(uint(request, "v"), uint(request, "nanos") as u32);
            json!([describe_time(time + std), describe_time(time - std)])
        }),
        "time_add_fixed" => on_time(&|time| match FixedOffset::east_opt(v as i32) {
            Some(offset) => json!([describe_time(time + offset), describe_time(time - offset)]),
            None => Value::Null,
        }),
        // NaiveDateTime.
        "naive" => on_naive(&describe_naive),
        "naive_add_signed" => on_naive(&|naive| d.map_or(Value::Null, |d| describe_opt_naive(naive.checked_add_signed(d)))),
        "naive_sub_signed" => on_naive(&|naive| d.map_or(Value::Null, |d| describe_opt_naive(naive.checked_sub_signed(d)))),
        "naive_plus" => on_naive(&|naive| d.map_or(Value::Null, |d| describe_naive(naive + d))),
        "naive_minus" => on_naive(&|naive| d.map_or(Value::Null, |d| describe_naive(naive - d))),
        "naive_add_offset" => on_naive(&|naive| match FixedOffset::east_opt(v as i32) {
            Some(offset) => json!([
                describe_opt_naive(naive.checked_add_offset(offset)),
                describe_opt_naive(naive.checked_sub_offset(offset)),
                catch(|| describe_naive(naive + offset)),
                catch(|| describe_naive(naive - offset))
            ]),
            None => Value::Null,
        }),
        "naive_months" => on_naive(&|naive| {
            json!([
                describe_opt_naive(naive.checked_add_months(Months::new(v as u32))),
                describe_opt_naive(naive.checked_sub_months(Months::new(v as u32))),
                catch(|| describe_naive(naive + Months::new(v as u32))),
                catch(|| describe_naive(naive - Months::new(v as u32)))
            ])
        }),
        "naive_days" => on_naive(&|naive| {
            json!([
                describe_opt_naive(naive.checked_add_days(Days::new(uint(request, "v")))),
                describe_opt_naive(naive.checked_sub_days(Days::new(uint(request, "v")))),
                catch(|| describe_naive(naive + Days::new(uint(request, "v")))),
                catch(|| describe_naive(naive - Days::new(uint(request, "v"))))
            ])
        }),
        "naive_since" => on_naive(&|naive| match (other_date, other_time) {
            (Some(date), Some(time)) => {
                let other = date.and_time(time);
                json!([describe_delta(naive.signed_duration_since(other)), naive.cmp(&other) as i8])
            }
            _ => Value::Null,
        }),
        "naive_with" => on_naive(&|naive| {
            json!([
                describe_opt_naive(naive.with_year(v as i32)),
                describe_opt_naive(naive.with_month(v as u32)),
                describe_opt_naive(naive.with_day(v as u32)),
                describe_opt_naive(naive.with_ordinal(v as u32)),
                describe_opt_naive(naive.with_hour(v as u32)),
                describe_opt_naive(naive.with_minute(v as u32)),
                describe_opt_naive(naive.with_second(v as u32)),
                describe_opt_naive(naive.with_nanosecond(v as u32))
            ])
        }),
        "naive_round" => on_naive(&|naive| {
            let rounding = |result: Result<NaiveDateTime, chrono::RoundingError>| match result {
                Ok(naive) => json!({ "ok": describe_naive(naive) }),
                Err(error) => json!({ "err": error.to_string() }),
            };
            d.map_or(Value::Null, |d| {
                json!([
                    rounding(naive.duration_round(d)),
                    rounding(naive.duration_trunc(d)),
                    rounding(naive.duration_round_up(d)),
                    catch(|| describe_naive(naive.round_subsecs(v as u16))),
                    catch(|| describe_naive(naive.trunc_subsecs(v as u16)))
                ])
            })
        }),
        "naive_and_utc" => on_naive(&|naive| describe_dt(&naive.and_utc())),
        "month" => match Month::try_from(v as u8) {
            Ok(month) => json!({
                "ok": [
                    format!("{month:?}"),
                    month.name(),
                    month.number_from_month(),
                    format!("{:?}", month.succ()),
                    format!("{:?}", month.pred()),
                    option_json(month.num_days(y))
                ]
            }),
            Err(error) => json!({ "err": error.to_string(), "dbg": format!("{error:?}") }),
        },
        "weekday" => match Weekday::try_from(v as u8) {
            Ok(day) => json!({ "ok": [format!("{day:?}"), day.to_string(), day.days_since(weekday(int(request, "o")))] }),
            Err(error) => json!({ "err": error.to_string(), "dbg": format!("{error:?}") }),
        },
        _ => crate::unknown(request),
    }
}

fn tz(request: &Value) -> Value {
    match text(request, "a") {
        "parse" => match text(request, "s").parse::<Tz>() {
            Ok(tz) => json!({ "ok": [tz.name(), tz.to_string(), format!("{tz:?}")] }),
            Err(error) => json!({ "err": error.to_string(), "dbg": format!("{error:?}") }),
        },
        "list" => json!(chrono_tz::TZ_VARIANTS.iter().map(|tz| tz.name()).collect::<Vec<_>>()),
        "version" => json!(chrono_tz::IANA_TZDB_VERSION),
        "default" => json!(Tz::default().name()),
        "info" => {
            let Ok(tz) = text(request, "s").parse::<Tz>() else {
                return Value::Null;
            };
            let Some(utc) = utc_instant(request) else {
                return Value::Null;
            };
            let dt = utc.with_timezone(&tz);
            let offset = dt.offset();
            json!([
                offset.tz_id(),
                option_json(offset.abbreviation()),
                offset.base_utc_offset().num_seconds(),
                offset.dst_offset().num_seconds(),
                offset.fix().local_minus_utc(),
                offset.to_string(),
                format!("{offset:?}"),
                format!("{dt:?}"),
                dt.to_string(),
                dt.format("%Y-%m-%d %H:%M:%S %Z %z").to_string(),
                dt.timezone().name()
            ])
        }
        "gap" => {
            let Ok(tz) = text(request, "s").parse::<Tz>() else {
                return Value::Null;
            };
            let Some(naive) = naive_arg(request) else {
                return Value::Null;
            };
            match chrono_tz::GapInfo::new(&naive, &tz) {
                None => Value::Null,
                Some(gap) => json!([
                    gap.begin.map_or(Value::Null, |(naive, offset)| json!([format!("{naive:?}"), offset.to_string()])),
                    gap.end.map_or(Value::Null, |dt| describe_dt(&dt))
                ]),
            }
        }
        _ => crate::unknown(request),
    }
}

fn serde_text<T: serde::Serialize>(value: &T) -> Value {
    match serde_json::to_string(value) {
        Ok(encoded) => json!(encoded),
        Err(error) => json!({ "err": error.to_string() }),
    }
}

fn deserialize(request: &Value) -> Value {
    let Some(input) = request.get("s") else {
        return crate::unknown(request);
    };
    fn run<T: serde::de::DeserializeOwned>(input: &Value, describe: impl FnOnce(T) -> Value) -> Value {
        match serde_json::from_value::<T>(input.clone()) {
            Ok(value) => json!({ "ok": describe(value) }),
            Err(error) => json!({ "err": error.to_string() }),
        }
    }
    match text(request, "t") {
        "dt_fixed" => run::<DateTime<FixedOffset>>(input, |dt| describe_dt(&dt)),
        "dt_utc" => run::<DateTime<Utc>>(input, |dt| describe_dt(&dt)),
        "dt_local" => run::<DateTime<Local>>(input, |dt| describe_dt(&dt)),
        "ndt" => run::<NaiveDateTime>(input, describe_naive),
        "nd" => run::<NaiveDate>(input, describe_date),
        "nt" => run::<NaiveTime>(input, describe_time),
        "delta" => run::<TimeDelta>(input, describe_delta),
        "weekday" => run::<Weekday>(input, |day| json!(format!("{day:?}"))),
        "month" => run::<Month>(input, |month| json!(format!("{month:?}"))),
        _ => crate::unknown(request),
    }
}

fn serialize(request: &Value) -> Value {
    match text(request, "t") {
        "nd" => date_arg(request, "days").map_or(Value::Null, |date| serde_text(&date)),
        "nt" => time_arg(request, "tsecs", "frac").map_or(Value::Null, |time| serde_text(&time)),
        "ndt" => naive_arg(request).map_or(Value::Null, |naive| serde_text(&naive)),
        "delta" => delta_arg(request.get("d")).map_or(Value::Null, |delta| serde_text(&delta)),
        "weekday" => serde_text(&weekday(int(request, "v"))),
        "month" => Month::try_from(int(request, "v") as u8).map_or(Value::Null, |month| serde_text(&month)),
        _ => crate::unknown(request),
    }
}

fn dispatch(request: &Value) -> Value {
    match text(request, "op") {
        "text" => instant(request, "text"),
        "fields" => instant(request, "fields"),
        "fmt" => format(request),
        "items" => {
            let spec = text(request, "f");
            let lenient = request.get("lenient").and_then(Value::as_bool).unwrap_or(false);
            let items = if lenient { StrftimeItems::new_lenient(spec) } else { StrftimeItems::new(spec) };
            let parsed = match StrftimeItems::new(spec).parse() {
                Ok(items) => json!(items.len()),
                Err(error) => json!(error.to_string()),
            };
            json!([items.map(|item| describe_item(&item)).collect::<Vec<_>>(), parsed])
        }
        "parse" => parse(request),
        "delta" => delta(request),
        "arith" => arith(request),
        "zone" => zone_op(request),
        "naive" => naive(request),
        "tz" => tz(request),
        "de" => deserialize(request),
        "ser" => serialize(request),
        _ => crate::unknown(request),
    }
}

/// The value `TZ` gets for a request's `tzenv` member.
fn tz_value(spec: &str) -> String {
    let absolute = |relative: &str| {
        std::env::current_dir().expect("working directory").join(relative).to_string_lossy().into_owned()
    };
    if let Some(relative) = spec.strip_prefix("@:") {
        format!(":{}", absolute(relative))
    } else if let Some(relative) = spec.strip_prefix('@') {
        absolute(relative)
    } else {
        spec.to_owned()
    }
}

pub fn handle(request: &Value) -> Value {
    quiet();
    let Some(tz_env) = request.get("tzenv") else {
        return dispatch(request);
    };
    // SAFETY: the oracle serves one request at a time, and no other thread
    // reads the environment while this one changes it.
    unsafe {
        match tz_env.as_str() {
            Some(spec) => std::env::set_var("TZ", tz_value(spec)),
            None => std::env::remove_var("TZ"),
        }
    }
    // chrono caches the local zone per thread and looks at `TZ` again only
    // after a second, so every request gets a thread of its own.
    let joined = std::thread::scope(|scope| scope.spawn(|| catch(|| dispatch(request))).join());
    joined.unwrap_or_else(|payload| json!({ "panic": panic_message(payload) }))
}
