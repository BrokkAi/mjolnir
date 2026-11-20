//! Oracle module `std_time`: `std::time`, and the fixed text of `std::sync` and
//! `std::thread`, as the pinned toolchain produces them.
//!
//! Requests are objects with an `"op"` member:
//!
//! - `dur_debug`: `Debug` text of a `Duration` under one of a fixed set of
//!   format specs (`fmt`, with `width` and `prec`).
//! - `dur_from_f64`, `dur_from_f32`: `Duration::try_from_secs_f64/f32` of a
//!   float given as its bits.
//! - `dur_ctor`, `dur_methods`, `dur_binary`, `dur_scalar`, `dur_float`,
//!   `dur_sum`: constructors, accessors and arithmetic.
//! - `systime`: `SystemTime` arithmetic and its error.
//! - `text`: one fixed text (an error's `Display` or `Debug`, a panic message)
//!   named by `key`.
//!
//! The Go port holds a `Duration` as `time.Duration`, whose largest value is
//! `i64::MAX` nanoseconds. Where a reply could exceed that, it is given in the
//! form the port must produce: conversions saturate (`sat`), and arithmetic
//! whose exact result is out of the port's range counts as overflow. The text
//! of an overflow panic is always the one the standard library itself raises.
//!
//! The Go tests that use this module are in go/rs/rstime, go/rs/rssync and
//! go/rs/rsthread.

use std::panic::{AssertUnwindSafe, catch_unwind};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{
    Arc, Barrier, Condvar, LazyLock, Mutex, Once, OnceLock, RwLock, mpsc,
};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use serde_json::{Value, json};

/// The largest duration the Go port can hold, in nanoseconds.
const GO_MAX_NANOS: u128 = i64::MAX as u128;

pub fn handle(request: &Value) -> Value {
    match crate::text(request, "op") {
        "dur_debug" => dur_debug(request),
        "dur_from_f64" => dur_from_f64(request),
        "dur_from_f32" => dur_from_f32(request),
        "dur_ctor" => dur_ctor(request),
        "dur_methods" => dur_methods(request),
        "dur_binary" => dur_binary(request),
        "dur_scalar" => dur_scalar(request),
        "dur_float" => dur_float(request),
        "dur_sum" => dur_sum(request),
        "systime" => systime(request),
        "text" => match fixed_text(crate::text(request, "key")) {
            Some(text) => json!({ "text": text }),
            None => crate::unknown(request),
        },
        _ => crate::unknown(request),
    }
}

fn uint(request: &Value, key: &str) -> u64 {
    match request.get(key) {
        Some(Value::String(text)) => text.parse().expect("unsigned integer"),
        Some(value) => value.as_u64().expect("unsigned integer"),
        None => 0,
    }
}

fn int(request: &Value, key: &str) -> i64 {
    request.get(key).and_then(Value::as_i64).unwrap_or(0)
}

/// Runs `f` and returns the message of the panic it raises, without the panic
/// hook printing it.
fn panic_message(f: impl FnOnce()) -> Option<String> {
    let hook = std::panic::take_hook();
    std::panic::set_hook(Box::new(|_| {}));
    let result = catch_unwind(AssertUnwindSafe(f));
    std::panic::set_hook(hook);
    let payload = result.err()?;
    Some(
        payload
            .downcast_ref::<String>()
            .cloned()
            .or_else(|| payload.downcast_ref::<&str>().map(|text| (*text).to_owned()))
            .unwrap_or_else(|| "non-string panic payload".to_owned()),
    )
}

/// `{"ok": value}` or `{"panic": message}` for a computation that may panic.
fn ok_or_panic(f: impl FnOnce() -> Value) -> Value {
    let mut value = None;
    match panic_message(|| value = Some(f())) {
        Some(message) => json!({ "panic": message }),
        None => json!({ "ok": value }),
    }
}

fn parts(duration: Duration) -> Value {
    json!([duration.as_secs(), duration.subsec_nanos()])
}

/// A duration as the port holds it: nanoseconds, saturated at the port's maximum.
fn sat(duration: Duration) -> Value {
    json!(duration.as_nanos().min(GO_MAX_NANOS) as u64)
}

/// A duration the port can hold exactly, or null.
fn in_range(duration: Option<Duration>) -> Value {
    match duration {
        Some(duration) if duration.as_nanos() <= GO_MAX_NANOS => json!(duration.as_nanos() as u64),
        _ => Value::Null,
    }
}

fn dur_debug(request: &Value) -> Value {
    let d = Duration::new(uint(request, "secs"), uint(request, "nanos") as u32);
    let w = uint(request, "width") as usize;
    let p = uint(request, "prec") as usize;
    let text = match crate::text(request, "fmt") {
        "plain" => format!("{d:?}"),
        "alt" => format!("{d:#?}"),
        "prec" => format!("{d:.p$?}"),
        "width" => format!("{d:w$?}"),
        "width_prec" => format!("{d:w$.p$?}"),
        "left" => format!("{d:<w$?}"),
        "right" => format!("{d:>w$?}"),
        "center" => format!("{d:^w$?}"),
        "left_prec" => format!("{d:<w$.p$?}"),
        "right_prec" => format!("{d:>w$.p$?}"),
        "center_prec" => format!("{d:^w$.p$?}"),
        "fill_left" => format!("{d:*<w$?}"),
        "fill_right" => format!("{d:*>w$.p$?}"),
        "fill_center" => format!("{d:é^w$.p$?}"),
        "plus" => format!("{d:+?}"),
        "plus_prec" => format!("{d:+.p$?}"),
        "plus_width" => format!("{d:+w$?}"),
        "plus_right" => format!("{d:>+w$.p$?}"),
        "zero" => format!("{d:0w$?}"),
        "zero_prec" => format!("{d:0w$.p$?}"),
        _ => return crate::unknown(request),
    };
    json!({ "text": text })
}

fn float_result(result: Result<Duration, std::time::TryFromFloatSecsError>) -> Value {
    match result {
        Ok(duration) => json!({ "ok": parts(duration), "sat": sat(duration) }),
        Err(error) => json!({ "err": error.to_string() }),
    }
}

fn dur_from_f64(request: &Value) -> Value {
    float_result(Duration::try_from_secs_f64(f64::from_bits(uint(request, "bits"))))
}

fn dur_from_f32(request: &Value) -> Value {
    float_result(Duration::try_from_secs_f32(f32::from_bits(uint(request, "bits") as u32)))
}

fn dur_ctor(request: &Value) -> Value {
    let value = uint(request, "value");
    let nanos = uint(request, "nanos") as u32;
    json!({
        "from_secs": sat(Duration::from_secs(value)),
        "from_millis": sat(Duration::from_millis(value)),
        "from_micros": sat(Duration::from_micros(value)),
        "from_nanos": sat(Duration::from_nanos(value)),
        "from_mins": ok_or_panic(|| sat(Duration::from_mins(value))),
        "from_hours": ok_or_panic(|| sat(Duration::from_hours(value))),
        "new": ok_or_panic(|| sat(Duration::new(value, nanos))),
    })
}

fn dur_methods(request: &Value) -> Value {
    let d = Duration::from_nanos(uint(request, "nanos"));
    // `as_millis_f64` and `as_millis_f32` are unstable; these are their bodies.
    let millis_f64 = (d.as_secs() as f64) * 1_000.0 + (d.subsec_nanos() as f64) / 1_000_000.0;
    let millis_f32 = (d.as_secs() as f32) * 1_000.0 + (d.subsec_nanos() as f32) / 1_000_000.0;
    json!({
        "as_secs": d.as_secs(),
        "subsec_millis": d.subsec_millis(),
        "subsec_micros": d.subsec_micros(),
        "subsec_nanos": d.subsec_nanos(),
        "as_millis": d.as_millis() as u64,
        "as_micros": d.as_micros() as u64,
        "as_nanos": d.as_nanos() as u64,
        "as_secs_f64": d.as_secs_f64().to_bits().to_string(),
        "as_secs_f32": d.as_secs_f32().to_bits(),
        "as_millis_f64": millis_f64.to_bits().to_string(),
        "as_millis_f32": millis_f32.to_bits(),
        "is_zero": d.is_zero(),
        "debug": format!("{d:?}"),
    })
}

/// The message of the panic the standard library raises for `f`, which must
/// overflow.
fn overflow_text(f: impl FnOnce()) -> Value {
    json!({ "panic": panic_message(f).expect("the operation overflows") })
}

/// An operator's result in the port: its value when the port can hold it, and
/// otherwise the panic the standard library raises when the operator overflows.
fn operator(result: Option<Duration>, overflow: impl FnOnce()) -> Value {
    match result {
        Some(duration) if duration.as_nanos() <= GO_MAX_NANOS => {
            json!({ "ok": duration.as_nanos() as u64 })
        }
        _ => overflow_text(overflow),
    }
}

fn dur_binary(request: &Value) -> Value {
    let a = Duration::from_nanos(uint(request, "a"));
    let b = Duration::from_nanos(uint(request, "b"));
    json!({
        "checked_add": in_range(a.checked_add(b)),
        "checked_sub": in_range(a.checked_sub(b)),
        "saturating_add": sat(a.saturating_add(b)),
        "saturating_sub": sat(a.saturating_sub(b)),
        "abs_diff": sat(a.abs_diff(b)),
        "add": operator(a.checked_add(b), || {
            let _ = std::hint::black_box(Duration::MAX) + Duration::from_nanos(1);
        }),
        "sub": operator(a.checked_sub(b), || {
            let _ = std::hint::black_box(Duration::ZERO) - Duration::from_nanos(1);
        }),
        "div_duration_f64": a.div_duration_f64(b).to_bits().to_string(),
        "div_duration_f32": a.div_duration_f32(b).to_bits(),
        "cmp": a.cmp(&b) as i8,
    })
}

fn dur_scalar(request: &Value) -> Value {
    let a = Duration::from_nanos(uint(request, "a"));
    let rhs = uint(request, "rhs") as u32;
    json!({
        "checked_mul": in_range(a.checked_mul(rhs)),
        "saturating_mul": sat(a.saturating_mul(rhs)),
        "checked_div": in_range(a.checked_div(rhs)),
        "mul": operator(a.checked_mul(rhs), || {
            let _ = std::hint::black_box(Duration::MAX) * 2;
        }),
        "div": operator(a.checked_div(rhs), || {
            let _ = Duration::from_secs(1) / std::hint::black_box(0);
        }),
    })
}

fn dur_float(request: &Value) -> Value {
    let a = Duration::from_nanos(uint(request, "a"));
    let wide = f64::from_bits(uint(request, "bits"));
    let narrow = f32::from_bits(uint(request, "bits32") as u32);
    let value = |d: Duration| json!({ "parts": parts(d), "sat": sat(d) });
    json!({
        "mul_f64": ok_or_panic(|| value(a.mul_f64(wide))),
        "div_f64": ok_or_panic(|| value(a.div_f64(wide))),
        "mul_f32": ok_or_panic(|| value(a.mul_f32(narrow))),
        "div_f32": ok_or_panic(|| value(a.div_f32(narrow))),
        "from_secs_f64": ok_or_panic(|| value(Duration::from_secs_f64(wide))),
        "from_secs_f32": ok_or_panic(|| value(Duration::from_secs_f32(narrow))),
    })
}

fn dur_sum(request: &Value) -> Value {
    let items: Vec<Duration> = request
        .get("items")
        .and_then(Value::as_array)
        .map(|items| {
            items.iter().map(|item| Duration::from_nanos(item.as_u64().unwrap_or(0))).collect()
        })
        .unwrap_or_default();
    let exact: u128 = items.iter().map(Duration::as_nanos).sum();
    if exact <= GO_MAX_NANOS {
        let total: Duration = items.iter().sum();
        json!({ "ok": total.as_nanos() as u64 })
    } else {
        overflow_text(|| {
            let _: Duration = [Duration::MAX, Duration::MAX].iter().sum();
        })
    }
}

/// A `SystemTime` from seconds (which may be negative) and nanoseconds after
/// the Unix epoch.
fn system_time(secs: i64, nanos: u32) -> SystemTime {
    let base = if secs >= 0 {
        UNIX_EPOCH + Duration::from_secs(secs as u64)
    } else {
        UNIX_EPOCH - Duration::from_secs(secs.unsigned_abs())
    };
    base + Duration::from_nanos(nanos as u64)
}

/// A `SystemTime` as seconds and nanoseconds after the Unix epoch.
fn system_time_parts(time: SystemTime) -> Value {
    match time.duration_since(UNIX_EPOCH) {
        Ok(after) => json!([after.as_secs() as i64, after.subsec_nanos()]),
        Err(before) => {
            let before = before.duration();
            if before.subsec_nanos() == 0 {
                json!([-(before.as_secs() as i64), 0])
            } else {
                json!([-(before.as_secs() as i64) - 1, 1_000_000_000 - before.subsec_nanos()])
            }
        }
    }
}

fn systime(request: &Value) -> Value {
    let a = system_time(int(request, "a_secs"), uint(request, "a_nanos") as u32);
    let b = system_time(int(request, "b_secs"), uint(request, "b_nanos") as u32);
    let d = Duration::from_nanos(uint(request, "d"));
    let since = match a.duration_since(b) {
        Ok(duration) => json!({ "ok": sat(duration) }),
        Err(error) => json!({ "err": sat(error.duration()), "display": error.to_string() }),
    };
    json!({
        "duration_since": since,
        "checked_add": a.checked_add(d).map(system_time_parts),
        "checked_sub": a.checked_sub(d).map(system_time_parts),
        "cmp": a.cmp(&b) as i8,
        "debug": format!("{a:?}"),
    })
}

/// Replaces every run of digits with `N`, for text that holds a value that
/// differs from run to run.
fn digits_as_n(text: &str) -> String {
    let mut out = String::new();
    let mut in_digits = false;
    for c in text.chars() {
        if c.is_ascii_digit() {
            if !in_digits {
                out.push('N');
            }
            in_digits = true;
        } else {
            in_digits = false;
            out.push(c);
        }
    }
    out
}

fn poisoned_mutex() -> Arc<Mutex<i32>> {
    let mutex = Arc::new(Mutex::new(7));
    let held = Arc::clone(&mutex);
    panic_message(move || {
        let _ = std::thread::spawn(move || {
            let _guard = held.lock().unwrap();
            panic!("poison the mutex");
        })
        .join();
    });
    assert!(mutex.is_poisoned());
    mutex
}

fn poisoned_rwlock() -> Arc<RwLock<i32>> {
    let lock = Arc::new(RwLock::new(7));
    let held = Arc::clone(&lock);
    panic_message(move || {
        let _ = std::thread::spawn(move || {
            let _guard = held.write().unwrap();
            panic!("poison the lock");
        })
        .join();
    });
    assert!(lock.is_poisoned());
    lock
}

fn panic_text(f: impl FnOnce()) -> Option<String> {
    Some(panic_message(f).expect("the operation panics"))
}

fn fixed_text(key: &str) -> Option<String> {
    let float_negative = Duration::try_from_secs_f64(-1.0).unwrap_err();
    let float_overflow = Duration::try_from_secs_f64(f64::NAN).unwrap_err();
    match key {
        // std::time
        "systemtime_error_display" => {
            Some(UNIX_EPOCH.duration_since(UNIX_EPOCH + Duration::from_millis(1500)).unwrap_err().to_string())
        }
        "systemtime_error_debug" => Some(format!(
            "{:?}",
            UNIX_EPOCH.duration_since(UNIX_EPOCH + Duration::from_millis(1500)).unwrap_err()
        )),
        "systemtime_error_padded" => Some(format!(
            "[{:>50}]",
            UNIX_EPOCH.duration_since(UNIX_EPOCH + Duration::from_millis(1500)).unwrap_err()
        )),
        "systemtime_duration_since_unwrap_panic" => panic_text(|| {
            UNIX_EPOCH.duration_since(UNIX_EPOCH + Duration::from_millis(1500)).unwrap();
        }),
        "try_from_float_negative_display" => Some(float_negative.to_string()),
        "try_from_float_negative_debug" => Some(format!("{float_negative:?}")),
        "try_from_float_overflow_display" => Some(float_overflow.to_string()),
        "try_from_float_overflow_debug" => Some(format!("{float_overflow:?}")),
        "try_from_float_padded" => Some(format!("[{float_negative:>70}]")),
        "instant_add_overflow_panic" => panic_text(|| {
            let _ = Instant::now() + std::hint::black_box(Duration::MAX);
        }),
        "instant_sub_overflow_panic" => panic_text(|| {
            let _ = Instant::now() - std::hint::black_box(Duration::MAX);
        }),
        "systemtime_add_overflow_panic" => panic_text(|| {
            let _ = SystemTime::now() + std::hint::black_box(Duration::MAX);
        }),
        "systemtime_sub_overflow_panic" => panic_text(|| {
            let _ = SystemTime::now() - std::hint::black_box(Duration::MAX);
        }),
        "duration_add_overflow_panic" => panic_text(|| {
            let _ = std::hint::black_box(Duration::MAX) + Duration::from_nanos(1);
        }),
        "duration_sub_overflow_panic" => panic_text(|| {
            let _ = std::hint::black_box(Duration::ZERO) - Duration::from_nanos(1);
        }),
        "duration_mul_overflow_panic" => panic_text(|| {
            let _ = std::hint::black_box(Duration::MAX) * 2;
        }),
        "duration_div_zero_panic" => panic_text(|| {
            let _ = Duration::from_secs(1) / std::hint::black_box(0);
        }),
        "duration_new_overflow_panic" => panic_text(|| {
            let _ = Duration::new(std::hint::black_box(u64::MAX), 1_000_000_000);
        }),
        "duration_from_mins_overflow_panic" => panic_text(|| {
            let _ = Duration::from_mins(std::hint::black_box(u64::MAX));
        }),
        "duration_from_hours_overflow_panic" => panic_text(|| {
            let _ = Duration::from_hours(std::hint::black_box(u64::MAX));
        }),
        "duration_sum_overflow_panic" => panic_text(|| {
            let _: Duration = [Duration::MAX, Duration::MAX].iter().sum();
        }),
        "duration_from_secs_f64_negative_panic" => panic_text(|| {
            let _ = Duration::from_secs_f64(std::hint::black_box(-1.0));
        }),
        "duration_from_secs_f64_nan_panic" => panic_text(|| {
            let _ = Duration::from_secs_f64(std::hint::black_box(f64::NAN));
        }),
        "unix_epoch_debug" => Some(format!("{UNIX_EPOCH:?}")),
        "systemtime_debug_pretty" => Some(format!("{:#?}", UNIX_EPOCH + Duration::new(5, 6))),
        "instant_debug_shape" => Some(digits_as_n(&format!("{:?}", Instant::now()))),

        // std::sync: poisoning
        "poison_error_display" => Some(poisoned_mutex().lock().unwrap_err().to_string()),
        "poison_error_debug" => Some(format!("{:?}", poisoned_mutex().lock().unwrap_err())),
        "poison_error_padded" => Some(format!("[{:>50}]", poisoned_mutex().lock().unwrap_err())),
        "try_lock_error_would_block_display" => {
            let mutex = Mutex::new(1);
            let _held = mutex.lock().unwrap();
            Some(mutex.try_lock().unwrap_err().to_string())
        }
        "try_lock_error_would_block_debug" => {
            let mutex = Mutex::new(1);
            let _held = mutex.lock().unwrap();
            Some(format!("{:?}", mutex.try_lock().unwrap_err()))
        }
        "try_lock_error_poisoned_display" => Some(poisoned_mutex().try_lock().unwrap_err().to_string()),
        "try_lock_error_poisoned_debug" => Some(format!("{:?}", poisoned_mutex().try_lock().unwrap_err())),
        "mutex_lock_unwrap_poisoned_panic" => panic_text(|| {
            drop(poisoned_mutex().lock().unwrap());
        }),
        "mutex_lock_expect_poisoned_panic" => panic_text(|| {
            drop(poisoned_mutex().lock().expect("the lock is sound"));
        }),
        "mutex_try_lock_unwrap_would_block_panic" => panic_text(|| {
            let mutex = Mutex::new(1);
            let _held = mutex.lock().unwrap();
            drop(mutex.try_lock().unwrap());
        }),
        "mutex_try_lock_unwrap_poisoned_panic" => panic_text(|| {
            drop(poisoned_mutex().try_lock().unwrap());
        }),
        "mutex_try_lock_expect_would_block_panic" => panic_text(|| {
            let mutex = Mutex::new(1);
            let _held = mutex.lock().unwrap();
            drop(mutex.try_lock().expect("the lock is free"));
        }),
        "mutex_into_inner_unwrap_poisoned_panic" => panic_text(|| {
            let mutex = Arc::try_unwrap(poisoned_mutex()).unwrap();
            let _ = mutex.into_inner().unwrap();
        }),
        "mutex_get_mut_unwrap_poisoned_panic" => panic_text(|| {
            let mut mutex = Arc::try_unwrap(poisoned_mutex()).unwrap();
            let _ = mutex.get_mut().unwrap();
        }),
        "rwlock_read_unwrap_poisoned_panic" => panic_text(|| {
            drop(poisoned_rwlock().read().unwrap());
        }),
        "rwlock_write_unwrap_poisoned_panic" => panic_text(|| {
            drop(poisoned_rwlock().write().unwrap());
        }),
        "rwlock_try_read_unwrap_would_block_panic" => panic_text(|| {
            let lock = RwLock::new(1);
            let _held = lock.write().unwrap();
            drop(lock.try_read().unwrap());
        }),
        "condvar_wait_unwrap_poisoned_panic" => panic_text(|| {
            let mutex = poisoned_mutex();
            let guard = mutex.lock().unwrap_or_else(|poisoned| poisoned.into_inner());
            let _ = Condvar::new().wait_timeout(guard, Duration::from_millis(1)).unwrap();
        }),
        "condvar_wait_expect_poisoned_panic" => panic_text(|| {
            let mutex = poisoned_mutex();
            let guard = mutex.lock().unwrap_or_else(|poisoned| poisoned.into_inner());
            let _ = Condvar::new().wait_timeout(guard, Duration::from_millis(1)).expect("the wait is sound");
        }),

        // std::sync: Debug of the primitives
        "mutex_debug_unlocked" => Some(format!("{:?}", Mutex::new(5))),
        "mutex_debug_pretty" => Some(format!("{:#?}", Mutex::new(5))),
        "mutex_debug_locked" => {
            let mutex = Mutex::new(5);
            let _held = mutex.lock().unwrap();
            Some(format!("{mutex:?}"))
        }
        "mutex_debug_poisoned" => Some(format!("{:?}", poisoned_mutex())),
        "mutex_guard_debug" => Some(format!("{:?}", Mutex::new("text").lock().unwrap())),
        "mutex_guard_display" => Some(format!("{}", Mutex::new("text").lock().unwrap())),
        "rwlock_debug_unlocked" => Some(format!("{:?}", RwLock::new(5))),
        "rwlock_debug_read_locked" => {
            let lock = RwLock::new(5);
            let _held = lock.read().unwrap();
            Some(format!("{lock:?}"))
        }
        "rwlock_debug_write_locked" => {
            let lock = RwLock::new(5);
            let _held = lock.write().unwrap();
            Some(format!("{lock:?}"))
        }
        "rwlock_debug_poisoned" => Some(format!("{:?}", poisoned_rwlock())),
        "rwlock_read_guard_debug" => Some(format!("{:?}", RwLock::new("text").read().unwrap())),
        "rwlock_write_guard_debug" => Some(format!("{:?}", RwLock::new("text").write().unwrap())),
        "condvar_debug" => Some(format!("{:?}", Condvar::new())),
        "barrier_debug" => Some(format!("{:?}", Barrier::new(1))),
        "barrier_wait_result_debug" => Some(format!("{:?}", Barrier::new(1).wait())),
        "once_debug" => Some(format!("{:?}", Once::new())),
        "once_state_debug" => {
            let mut text = String::new();
            Once::new().call_once_force(|state| text = format!("{state:?}"));
            Some(text)
        }
        "wait_timeout_result_debug" => {
            let mutex = Mutex::new(());
            let (_guard, result) =
                Condvar::new().wait_timeout(mutex.lock().unwrap(), Duration::from_millis(1)).unwrap();
            Some(format!("{result:?}"))
        }
        "once_lock_debug_uninit" => Some(format!("{:?}", OnceLock::<i32>::new())),
        "once_lock_debug_init" => Some(format!("{:?}", OnceLock::from("text"))),
        "once_lock_set_unwrap_panic" => panic_text(|| {
            let cell = OnceLock::from(1);
            cell.set(2).unwrap();
        }),
        "once_lock_set_expect_panic" => panic_text(|| {
            let cell = OnceLock::from(1);
            cell.set(2).expect("set once");
        }),
        "lazy_lock_debug_uninit" => Some(format!("{:?}", LazyLock::new(|| 5))),
        "lazy_lock_debug_init" => {
            let lazy = LazyLock::new(|| 5);
            LazyLock::force(&lazy);
            Some(format!("{lazy:?}"))
        }
        "once_poisoned_panic" => panic_text(|| {
            let once = Once::new();
            let _ = catch_unwind(AssertUnwindSafe(|| once.call_once(|| panic!("first"))));
            once.call_once(|| {});
        }),
        "lazy_lock_poisoned_panic" => panic_text(|| {
            let lazy: LazyLock<i32> = LazyLock::new(|| panic!("first"));
            let _ = catch_unwind(AssertUnwindSafe(|| *lazy));
            let _ = *lazy;
        }),

        // std::sync::atomic
        "atomic_bool_debug" => Some(format!("{:?}", AtomicBool::new(true))),
        "atomic_u64_debug" => Some(format!("{:?}", AtomicU64::new(42))),
        "ordering_debug" => Some(format!(
            "{:?} {:?} {:?} {:?} {:?}",
            Ordering::Relaxed,
            Ordering::Release,
            Ordering::Acquire,
            Ordering::AcqRel,
            Ordering::SeqCst
        )),
        "atomic_load_release_panic" => panic_text(|| {
            AtomicU64::new(1).load(std::hint::black_box(Ordering::Release));
        }),
        "atomic_load_acqrel_panic" => panic_text(|| {
            AtomicU64::new(1).load(std::hint::black_box(Ordering::AcqRel));
        }),
        "atomic_store_acquire_panic" => panic_text(|| {
            AtomicU64::new(1).store(2, std::hint::black_box(Ordering::Acquire));
        }),
        "atomic_store_acqrel_panic" => panic_text(|| {
            AtomicU64::new(1).store(2, std::hint::black_box(Ordering::AcqRel));
        }),
        "atomic_cas_failure_acqrel_panic" => panic_text(|| {
            let _ = AtomicU64::new(1).compare_exchange(
                1,
                2,
                Ordering::SeqCst,
                std::hint::black_box(Ordering::AcqRel),
            );
        }),
        "atomic_cas_failure_release_panic" => panic_text(|| {
            let _ = AtomicU64::new(1).compare_exchange(
                1,
                2,
                Ordering::SeqCst,
                std::hint::black_box(Ordering::Release),
            );
        }),

        // std::sync::mpsc
        "send_error_display" => Some(closed_sender().send(1).unwrap_err().to_string()),
        "send_error_debug" => Some(format!("{:?}", closed_sender().send(1).unwrap_err())),
        "send_unwrap_panic" => panic_text(|| closed_sender().send(1).unwrap()),
        "send_expect_panic" => panic_text(|| closed_sender().send(1).expect("the receiver lives")),
        "recv_error_display" => Some(closed_receiver().recv().unwrap_err().to_string()),
        "recv_error_debug" => Some(format!("{:?}", closed_receiver().recv().unwrap_err())),
        "recv_unwrap_panic" => panic_text(|| {
            closed_receiver().recv().unwrap();
        }),
        "try_recv_error_empty_display" => Some(mpsc::TryRecvError::Empty.to_string()),
        "try_recv_error_empty_debug" => Some(format!("{:?}", mpsc::TryRecvError::Empty)),
        "try_recv_error_disconnected_display" => Some(mpsc::TryRecvError::Disconnected.to_string()),
        "try_recv_error_disconnected_debug" => Some(format!("{:?}", mpsc::TryRecvError::Disconnected)),
        "try_recv_unwrap_panic" => panic_text(|| {
            closed_receiver().try_recv().unwrap();
        }),
        "recv_timeout_error_timeout_display" => Some(mpsc::RecvTimeoutError::Timeout.to_string()),
        "recv_timeout_error_timeout_debug" => Some(format!("{:?}", mpsc::RecvTimeoutError::Timeout)),
        "recv_timeout_error_disconnected_display" => {
            Some(mpsc::RecvTimeoutError::Disconnected.to_string())
        }
        "recv_timeout_error_disconnected_debug" => {
            Some(format!("{:?}", mpsc::RecvTimeoutError::Disconnected))
        }
        "recv_timeout_unwrap_panic" => panic_text(|| {
            closed_receiver().recv_timeout(Duration::from_millis(1)).unwrap();
        }),
        "try_send_error_full_display" => Some(mpsc::TrySendError::Full(1).to_string()),
        "try_send_error_full_debug" => Some(format!("{:?}", mpsc::TrySendError::Full(1))),
        "try_send_error_disconnected_display" => Some(mpsc::TrySendError::Disconnected(1).to_string()),
        "try_send_error_disconnected_debug" => {
            Some(format!("{:?}", mpsc::TrySendError::Disconnected(1)))
        }
        "try_send_unwrap_panic" => panic_text(|| {
            let (sender, _receiver) = mpsc::sync_channel::<i32>(0);
            sender.try_send(1).unwrap();
        }),
        "sender_debug" => Some(format!("{:?}", mpsc::channel::<i32>().0)),
        "sync_sender_debug" => Some(format!("{:?}", mpsc::sync_channel::<i32>(1).0)),
        "receiver_debug" => Some(format!("{:?}", mpsc::channel::<i32>().1)),

        // std::thread
        "join_unwrap_panic" => panic_text(|| {
            std::thread::spawn(|| panic!("in the thread")).join().unwrap();
        }),
        "join_expect_panic" => panic_text(|| {
            std::thread::spawn(|| panic!("in the thread")).join().expect("the thread finishes");
        }),
        "scope_panic" => panic_text(|| {
            std::thread::scope(|scope| {
                scope.spawn(|| panic!("in the thread"));
            });
        }),
        "thread_name_nul_panic" => panic_text(|| {
            let _ = std::thread::Builder::new().name("ab\0cd".to_owned()).spawn(|| {});
        }),
        "thread_id_debug_shape" => Some(digits_as_n(&format!("{:?}", std::thread::current().id()))),
        "thread_debug_named" => Some(digits_as_n(&format!(
            "{:?}",
            std::thread::Builder::new()
                .name("worker".to_owned())
                .spawn(|| std::thread::current())
                .unwrap()
                .join()
                .unwrap()
        ))),
        "thread_debug_unnamed" => Some(digits_as_n(&format!(
            "{:?}",
            std::thread::spawn(|| std::thread::current()).join().unwrap()
        ))),
        "thread_debug_pretty" => Some(digits_as_n(&format!(
            "{:#?}",
            std::thread::Builder::new()
                .name("worker".to_owned())
                .spawn(|| std::thread::current())
                .unwrap()
                .join()
                .unwrap()
        ))),
        "builder_debug" => Some(format!(
            "{:?}",
            std::thread::Builder::new().name("worker".to_owned()).stack_size(4096)
        )),
        "builder_debug_default" => Some(format!("{:?}", std::thread::Builder::new())),
        "join_handle_debug" => {
            let handle = std::thread::spawn(|| {});
            let text = format!("{handle:?}");
            handle.join().unwrap();
            Some(text)
        }
        "scoped_join_handle_debug" => {
            Some(std::thread::scope(|scope| format!("{:?}", scope.spawn(|| {}))))
        }
        "main_thread_name" => Some(format!("{:?}", std::thread::current().name())),
        _ => None,
    }
}

fn closed_sender() -> mpsc::Sender<i32> {
    mpsc::channel().0
}

fn closed_receiver() -> mpsc::Receiver<i32> {
    mpsc::channel().1
}
