//! Oracle module `std_sort`: the sorting and searching methods of core and
//! alloc that the Go package `go/rs` reproduces (sort*.go). The Go tests
//! that drive it are `go/rs/sort_oracle_test.go`.
//!
//! Inputs are not sent element by element: a request names a pattern, a
//! length, a number of distinct keys and a seed, and both sides build the
//! same keys from them (`keys` below, `sortOracleKeys` in Go). Each element
//! carries its original index as a tag, so the order the sort leaves equal
//! keys in is visible in the reply.
//!
//! Requests:
//!
//! - `{"op":"sort","f":"unstable"|"stable"|"cached","ty":T,"pattern":..,
//!   "len":n,"distinct":d,"seed":s}` sorts (key, tag) elements of the Rust
//!   type `T` by key and replies the tags in their final order (`"tags"`,
//!   only up to 64 elements), their FNV-1a hash (`"hash"`) and, for the
//!   unstable sort, how many times the comparison ran (`"calls"`). The types
//!   are chosen to take each of the routes `sort_unstable` picks from the
//!   element type: `u32x2` is `(u32, u32)`, `u64x2` `(u64, u64)`,
//!   `u64_string` `(u64, String)`, `big` `(u64, [u64; 11])` (96 bytes),
//!   `huge` `(u64, [u64; 12])` (104 bytes) and `cell` `(u32, Cell<u32>)`.
//! - `{"op":"sort_random_cmp","ty":T,"len":n,"seed":s}` runs
//!   `sort_unstable_by` with a comparison that ignores its arguments and
//!   answers from a random sequence, which makes the result depend on every
//!   step the algorithm takes. Replies as `sort`, or with the panic.
//! - `{"op":"search","f":"binary_search"|"binary_search_by_key"|
//!   "partition_point","len":n,"distinct":d,"seed":s,"probes":[..]}` searches
//!   the ascending keys for each probe and replies, per probe, the result
//!   (`[true, i]` for `Ok(i)`, `[false, i]` for `Err(i)`, the index for
//!   `partition_point`) and the indices the closure was called with.

use std::cell::Cell;

use serde_json::{Value, json};

fn number(request: &Value, key: &str) -> u64 {
    request.get(key).and_then(Value::as_u64).unwrap_or(0)
}

/// splitmix64, the generator both sides use.
struct Rng(u64);

impl Rng {
    fn next(&mut self) -> u64 {
        self.0 = self.0.wrapping_add(0x9E37_79B9_7F4A_7C15);
        let mut z = self.0;
        z = (z ^ (z >> 30)).wrapping_mul(0xBF58_476D_1CE4_E5B9);
        z = (z ^ (z >> 27)).wrapping_mul(0x94D0_49BB_1331_11EB);
        z ^ (z >> 31)
    }
}

/// The keys of a request. `sortOracleKeys` in the Go test builds the same.
fn keys(request: &Value) -> Vec<u64> {
    let len = number(request, "len") as usize;
    let distinct = number(request, "distinct").max(1);
    let mut rng = Rng(number(request, "seed"));
    let n = len as u64;
    (0..n)
        .map(|i| match crate::text(request, "pattern") {
            "random" => rng.next() % distinct,
            "ascending" => i * distinct / n,
            "descending" => (n - 1 - i) * distinct / n,
            "sawtooth" => i % distinct,
            "equal" => 0,
            "pipe" => i.min(n - 1 - i) * distinct / n,
            "push_front" => {
                if i == n - 1 {
                    0
                } else {
                    i * distinct / n + 1
                }
            }
            other => panic!("oracle: unknown pattern {other:?}"),
        })
        .collect()
}

/// FNV-1a over the tags, as decimal text.
fn hash(tags: &[usize]) -> String {
    let mut h: u64 = 0xcbf2_9ce4_8422_2325;
    for tag in tags {
        for byte in (*tag as u64).to_le_bytes() {
            h ^= u64::from(byte);
            h = h.wrapping_mul(0x0000_0100_0000_01b3);
        }
    }
    h.to_string()
}

fn reply(tags: &[usize], calls: Option<u64>) -> Value {
    let mut reply = json!({ "hash": hash(tags) });
    if tags.len() <= 64 {
        reply["tags"] = json!(tags);
    }
    if let Some(calls) = calls {
        reply["calls"] = json!(calls);
    }
    reply
}

/// A (key, tag) element type and how to build and read it.
trait Element: Sized {
    fn make(key: u64, tag: usize) -> Self;
    fn key(&self) -> u64;
    fn tag(&self) -> usize;
}

impl Element for (u32, u32) {
    fn make(key: u64, tag: usize) -> Self {
        (key as u32, tag as u32)
    }
    fn key(&self) -> u64 {
        u64::from(self.0)
    }
    fn tag(&self) -> usize {
        self.1 as usize
    }
}

impl Element for (u64, u64) {
    fn make(key: u64, tag: usize) -> Self {
        (key, tag as u64)
    }
    fn key(&self) -> u64 {
        self.0
    }
    fn tag(&self) -> usize {
        self.1 as usize
    }
}

impl Element for (u64, String) {
    fn make(key: u64, tag: usize) -> Self {
        (key, tag.to_string())
    }
    fn key(&self) -> u64 {
        self.0
    }
    fn tag(&self) -> usize {
        self.1.parse().unwrap()
    }
}

impl Element for (u64, [u64; 11]) {
    fn make(key: u64, tag: usize) -> Self {
        let mut rest = [0; 11];
        rest[0] = tag as u64;
        (key, rest)
    }
    fn key(&self) -> u64 {
        self.0
    }
    fn tag(&self) -> usize {
        self.1[0] as usize
    }
}

impl Element for (u64, [u64; 12]) {
    fn make(key: u64, tag: usize) -> Self {
        let mut rest = [0; 12];
        rest[0] = tag as u64;
        (key, rest)
    }
    fn key(&self) -> u64 {
        self.0
    }
    fn tag(&self) -> usize {
        self.1[0] as usize
    }
}

impl Element for (u32, Cell<u32>) {
    fn make(key: u64, tag: usize) -> Self {
        (key as u32, Cell::new(tag as u32))
    }
    fn key(&self) -> u64 {
        u64::from(self.0)
    }
    fn tag(&self) -> usize {
        self.1.get() as usize
    }
}

fn sort_as<T: Element>(request: &Value) -> Value {
    let mut v: Vec<T> = keys(request).into_iter().enumerate().map(|(tag, key)| T::make(key, tag)).collect();
    let mut calls = 0u64;
    let counted = match crate::text(request, "f") {
        "unstable" => {
            v.sort_unstable_by(|a, b| {
                calls += 1;
                a.key().cmp(&b.key())
            });
            true
        }
        "stable" => {
            v.sort_by(|a, b| a.key().cmp(&b.key()));
            false
        }
        "cached" => {
            v.sort_by_cached_key(|e| e.key());
            false
        }
        _ => return crate::unknown(request),
    };
    let tags: Vec<usize> = v.iter().map(T::tag).collect();
    reply(&tags, counted.then_some(calls))
}

fn sort_random_cmp_as<T: Element>(request: &Value) -> Value {
    let len = number(request, "len") as usize;
    let mut v: Vec<T> = (0..len).map(|tag| T::make(0, tag)).collect();
    let mut rng = Rng(number(request, "seed"));
    let mut calls = 0u64;
    v.sort_unstable_by(|_, _| {
        calls += 1;
        match rng.next() % 3 {
            0 => std::cmp::Ordering::Less,
            1 => std::cmp::Ordering::Equal,
            _ => std::cmp::Ordering::Greater,
        }
    });
    let tags: Vec<usize> = v.iter().map(T::tag).collect();
    reply(&tags, Some(calls))
}

fn by_type(request: &Value, random: bool) -> Value {
    macro_rules! dispatch {
        ($($name:literal => $t:ty),*) => {
            match crate::text(request, "ty") {
                $($name => if random { sort_random_cmp_as::<$t>(request) } else { sort_as::<$t>(request) },)*
                _ => crate::unknown(request),
            }
        };
    }
    dispatch!(
        "u32x2" => (u32, u32),
        "u64x2" => (u64, u64),
        "u64_string" => (u64, String),
        "big" => (u64, [u64; 11]),
        "huge" => (u64, [u64; 12]),
        "cell" => (u32, Cell<u32>)
    )
}

fn search(request: &Value) -> Value {
    let mut ascending = request.clone();
    ascending["pattern"] = json!("ascending");
    let keys = keys(&ascending);
    // binary_search_by_key searches elements that are not themselves keys.
    let wrapped: Vec<(u64, usize)> = keys.iter().enumerate().map(|(tag, key)| (*key, tag)).collect();
    let probes: Vec<u64> = request
        .get("probes")
        .and_then(Value::as_array)
        .map(|probes| probes.iter().filter_map(Value::as_u64).collect())
        .unwrap_or_default();
    let mut results = Vec::new();
    let mut visits = Vec::new();
    for x in probes {
        let mut visited: Vec<usize> = Vec::new();
        let result = match crate::text(request, "f") {
            "binary_search" => {
                let base = keys.as_ptr() as usize;
                let result = keys.binary_search_by(|k| {
                    visited.push((k as *const u64 as usize - base) / size_of::<u64>());
                    k.cmp(&x)
                });
                // binary_search is binary_search_by(|p| p.cmp(x)); the probes
                // are taken from the _by form, and the result from the real one.
                assert_eq!(result, keys.binary_search(&x));
                json!(match result {
                    Ok(index) => [json!(true), json!(index)],
                    Err(index) => [json!(false), json!(index)],
                })
            }
            "binary_search_by_key" => {
                let result = wrapped.binary_search_by_key(&x, |e| {
                    visited.push(e.1);
                    e.0
                });
                json!(match result {
                    Ok(index) => [json!(true), json!(index)],
                    Err(index) => [json!(false), json!(index)],
                })
            }
            "partition_point" => json!(wrapped.partition_point(|e| {
                visited.push(e.1);
                e.0 < x
            })),
            _ => return crate::unknown(request),
        };
        results.push(result);
        visits.push(visited);
    }
    json!({ "results": results, "visits": visits })
}

pub fn handle(request: &Value) -> Value {
    // The random comparison panics on purpose; the reply carries the
    // message, so the default hook's report on standard error is only noise.
    static QUIET: std::sync::Once = std::sync::Once::new();
    QUIET.call_once(|| std::panic::set_hook(Box::new(|_| {})));
    match crate::text(request, "op") {
        "sort" => by_type(request, false),
        "sort_random_cmp" => by_type(request, true),
        "search" => search(request),
        _ => crate::unknown(request),
    }
}
