//! Oracle module `std_collections`: runs scripts of operations on the real
//! `std::collections` types, for `go/rs/collections`.
//!
//! A request names a collection and carries a list of steps; each step is an
//! array whose first element is the operation. The reply holds, for every
//! step, what the operation returned and the state of the collection after
//! it, so the Go port is compared after each step and not only at the end.
//! A step that panics is answered with `{"panic": "<message>"}` in place of
//! its return value. The Go side of every operation is in
//! `go/rs/collections/oracle_test.go`.
//!
//! Requests:
//!
//! - `{"op": "btree_map" | "hash_map", "key": "int" | "str", "state": "full" | "digest", "steps": [..]}`
//! - `{"op": "btree_set" | "hash_set", "key": "int" | "str", "state": .., "steps": [..]}`
//! - `{"op": "vec_deque", "elem": "i64" | "u8", "state": .., "steps": [..]}`
//! - `{"op": "sip", "writes": [..]}`: `DefaultHasher` after each write
//! - `{"op": "hash_value"}`: `Hash` of a few fixed values, to pin how the
//!   derived impls feed a hasher
//!
//! The state is the whole content (`"full"`) or its length and an FNV-1a
//! digest of it (`"digest"`), which keeps the recorded corpus small for
//! large collections.

use std::collections::{BTreeMap, BTreeSet, HashMap, HashSet, VecDeque};
use std::hash::{Hash, Hasher};
use std::ops::Bound;
use std::panic::{AssertUnwindSafe, catch_unwind};

use serde_json::{Value, json};

pub fn handle(request: &Value) -> Value {
    // A panic inside a step is an expected result; the default hook would
    // write each one to standard error.
    static QUIET: std::sync::Once = std::sync::Once::new();
    QUIET.call_once(|| std::panic::set_hook(Box::new(|_| {})));

    let reply = match (crate::text(request, "op"), crate::text(request, "key")) {
        ("btree_map", "int") => script(request, BTreeMap::<i64, i64>::new(), btree_map_step, map_state),
        ("btree_map", "str") => {
            script(request, BTreeMap::<String, i64>::new(), btree_map_step, map_state)
        }
        ("btree_set", "int") => script(request, BTreeSet::<i64>::new(), btree_set_step, set_state),
        ("btree_set", "str") => {
            script(request, BTreeSet::<String>::new(), btree_set_step, set_state)
        }
        ("hash_map", "int") => {
            script(request, HashMap::<i64, i64>::new(), hash_map_step, hash_map_state)
        }
        ("hash_map", "str") => {
            script(request, HashMap::<String, i64>::new(), hash_map_step, hash_map_state)
        }
        ("hash_set", "int") => script(request, HashSet::<i64>::new(), hash_set_step, hash_set_state),
        ("hash_set", "str") => {
            script(request, HashSet::<String>::new(), hash_set_step, hash_set_state)
        }
        ("vec_deque", _) => match crate::text(request, "elem") {
            "i64" => script(request, VecDeque::<i64>::new(), vec_deque_step, vec_deque_state),
            "u8" => script(request, VecDeque::<u8>::new(), vec_deque_step, vec_deque_state),
            _ => None,
        },
        ("sip", _) => sip(request),
        ("hash_value", _) => Some(hash_value()),
        _ => None,
    };
    reply.unwrap_or_else(|| crate::unknown(request))
}

/// Runs the steps of a request on `collection`. `None` means a step was not
/// understood.
fn script<C>(
    request: &Value,
    mut collection: C,
    step: fn(&mut C, &str, &[Value]) -> Option<Value>,
    state: fn(&C, bool) -> Value,
) -> Option<Value> {
    let full = crate::text(request, "state") == "full";
    let mut results = Vec::new();
    for entry in request.get("steps")?.as_array()? {
        let entry = entry.as_array()?;
        let name = entry.first()?.as_str()?;
        let caught = catch_unwind(AssertUnwindSafe(|| step(&mut collection, name, &entry[1..])));
        let returned = match caught {
            Ok(returned) => returned?,
            Err(payload) => {
                let message = payload
                    .downcast_ref::<String>()
                    .cloned()
                    .or_else(|| payload.downcast_ref::<&str>().map(|text| (*text).to_owned()))
                    .unwrap_or_else(|| "non-string panic payload".to_owned());
                json!({ "panic": message })
            }
        };
        results.push(json!([returned, state(&collection, full)]));
    }
    Some(json!({ "steps": results }))
}

/// FNV-1a, 64 bits: the digest of a collection's content.
struct Fnv(u64);

impl Fnv {
    fn new() -> Self {
        Fnv(0xcbf2_9ce4_8422_2325)
    }

    fn write(&mut self, bytes: &[u8]) {
        for byte in bytes {
            self.0 ^= u64::from(*byte);
            self.0 = self.0.wrapping_mul(0x0000_0100_0000_01b3);
        }
    }

    fn text(&self) -> String {
        format!("{:016x}", self.0)
    }
}

/// A key or element type of the scripts.
trait Item: Ord + Clone + Hash + Eq + Default {
    fn parse(value: &Value) -> Option<Self>;
    fn json(&self) -> Value;
    fn feed(&self, digest: &mut Fnv);
    /// A number in `0..modulus` that the predicates of `retain` and
    /// `extract_if` are built on.
    fn class(&self, modulus: i64) -> i64;
    /// Two values with `.0 > .1`, for a range that is not one.
    fn inverted() -> (Self, Self);
    /// The value an `or_insert_with_key` closure derives from a key.
    fn derived(&self) -> i64 {
        self.class(1000)
    }
    fn add(&mut self, delta: i64);
    fn number(&self) -> i64;
}

impl Item for i64 {
    fn parse(value: &Value) -> Option<Self> {
        value.as_i64()
    }
    fn json(&self) -> Value {
        json!(self)
    }
    fn feed(&self, digest: &mut Fnv) {
        digest.write(&self.to_le_bytes());
    }
    fn class(&self, modulus: i64) -> i64 {
        self.rem_euclid(modulus)
    }
    fn inverted() -> (Self, Self) {
        (5, 3)
    }
    fn add(&mut self, delta: i64) {
        *self = self.wrapping_add(delta);
    }
    fn number(&self) -> i64 {
        *self
    }
}

impl Item for u8 {
    fn parse(value: &Value) -> Option<Self> {
        value.as_i64().map(|number| number as u8)
    }
    fn json(&self) -> Value {
        json!(self)
    }
    fn feed(&self, digest: &mut Fnv) {
        digest.write(&i64::from(*self).to_le_bytes());
    }
    fn class(&self, modulus: i64) -> i64 {
        i64::from(*self).rem_euclid(modulus)
    }
    fn inverted() -> (Self, Self) {
        (5, 3)
    }
    fn add(&mut self, delta: i64) {
        *self = (i64::from(*self).wrapping_add(delta)) as u8;
    }
    fn number(&self) -> i64 {
        i64::from(*self)
    }
}

impl Item for String {
    fn parse(value: &Value) -> Option<Self> {
        value.as_str().map(str::to_owned)
    }
    fn json(&self) -> Value {
        json!(self)
    }
    fn feed(&self, digest: &mut Fnv) {
        digest.write(&(self.len() as u64).to_le_bytes());
        digest.write(self.as_bytes());
    }
    fn class(&self, modulus: i64) -> i64 {
        let sum: i64 = self.bytes().map(i64::from).sum();
        (sum + self.len() as i64).rem_euclid(modulus)
    }
    fn inverted() -> (Self, Self) {
        ("m".to_owned(), "c".to_owned())
    }
    fn add(&mut self, _delta: i64) {}
    fn number(&self) -> i64 {
        self.len() as i64
    }
}

fn item<T: Item>(args: &[Value], index: usize) -> Option<T> {
    T::parse(args.get(index)?)
}

fn number(args: &[Value], index: usize) -> Option<i64> {
    args.get(index)?.as_i64()
}

fn index(args: &[Value], at: usize) -> Option<usize> {
    args.get(at)?.as_u64().map(|number| number as usize)
}

fn bound<T: Item>(args: &[Value], index: usize) -> Option<Bound<T>> {
    let parts = args.get(index)?.as_array()?;
    match parts.first()?.as_str()? {
        "i" => Some(Bound::Included(T::parse(parts.get(1)?)?)),
        "e" => Some(Bound::Excluded(T::parse(parts.get(1)?)?)),
        "u" => Some(Bound::Unbounded),
        _ => None,
    }
}

fn index_bound(args: &[Value], index: usize) -> Option<Bound<usize>> {
    let parts = args.get(index)?.as_array()?;
    let value = || parts.get(1).and_then(Value::as_u64).map(|number| number as usize);
    match parts.first()?.as_str()? {
        "i" => Some(Bound::Included(value()?)),
        "e" => Some(Bound::Excluded(value()?)),
        "u" => Some(Bound::Unbounded),
        _ => None,
    }
}

fn pairs<K: Item>(args: &[Value], index: usize) -> Option<Vec<(K, i64)>> {
    args.get(index)?
        .as_array()?
        .iter()
        .map(|pair| {
            let pair = pair.as_array()?;
            Some((K::parse(pair.first()?)?, pair.get(1)?.as_i64()?))
        })
        .collect()
}

fn items<T: Item>(args: &[Value], index: usize) -> Option<Vec<T>> {
    args.get(index)?.as_array()?.iter().map(T::parse).collect()
}

fn pair_json<K: Item>(key: &K, value: &i64) -> Value {
    json!([key.json(), value])
}

fn optional_pair<K: Item>(pair: Option<(&K, &i64)>) -> Value {
    pair.map_or(Value::Null, |(key, value)| pair_json(key, value))
}

fn optional_item<T: Item>(found: Option<&T>) -> Value {
    found.map_or(Value::Null, Item::json)
}

fn pair_list<'a, K: Item + 'a>(entries: impl Iterator<Item = (&'a K, &'a i64)>) -> Value {
    Value::Array(entries.map(|(key, value)| pair_json(key, value)).collect())
}

fn item_list<'a, T: Item + 'a>(elements: impl Iterator<Item = &'a T>) -> Value {
    Value::Array(elements.map(Item::json).collect())
}

fn ordering(order: std::cmp::Ordering) -> i64 {
    order as i64
}

fn map_state<K: Item>(map: &BTreeMap<K, i64>, full: bool) -> Value {
    if full {
        return pair_list(map.iter());
    }
    let mut digest = Fnv::new();
    for (key, value) in map {
        key.feed(&mut digest);
        digest.write(&value.to_le_bytes());
    }
    json!([map.len(), digest.text()])
}

fn set_state<T: Item>(set: &BTreeSet<T>, full: bool) -> Value {
    if full {
        return item_list(set.iter());
    }
    let mut digest = Fnv::new();
    for element in set {
        element.feed(&mut digest);
    }
    json!([set.len(), digest.text()])
}

fn hash_map_state<K: Item>(map: &HashMap<K, i64>, full: bool) -> Value {
    let sorted: BTreeMap<K, i64> = map.iter().map(|(key, value)| (key.clone(), *value)).collect();
    map_state(&sorted, full)
}

fn hash_set_state<T: Item>(set: &HashSet<T>, full: bool) -> Value {
    let sorted: BTreeSet<T> = set.iter().cloned().collect();
    set_state(&sorted, full)
}

/// Whether the map checks the bounds of a range: Rust does only when the
/// map has a root node, which an emptied map keeps and a new one lacks.
fn map_probe<K: Item>(map: &BTreeMap<K, i64>) -> bool {
    let (high, low) = K::inverted();
    catch_unwind(AssertUnwindSafe(|| map.range(high..low).count())).is_err()
}

fn set_probe<T: Item>(set: &BTreeSet<T>) -> bool {
    let (high, low) = T::inverted();
    catch_unwind(AssertUnwindSafe(|| set.range(high..low).count())).is_err()
}

fn btree_map_step<K: Item>(map: &mut BTreeMap<K, i64>, name: &str, args: &[Value]) -> Option<Value> {
    use std::collections::btree_map::Entry;
    Some(match name {
        "insert" => json!(map.insert(item(args, 0)?, number(args, 1)?)),
        "remove" => json!(map.remove(&item::<K>(args, 0)?)),
        "remove_entry" => map
            .remove_entry(&item::<K>(args, 0)?)
            .map_or(Value::Null, |(key, value)| pair_json(&key, &value)),
        "get" => json!(map.get(&item::<K>(args, 0)?)),
        "get_key_value" => optional_pair(map.get_key_value(&item::<K>(args, 0)?)),
        "contains_key" => json!(map.contains_key(&item::<K>(args, 0)?)),
        "get_mut_add" => {
            let delta = number(args, 1)?;
            json!(map.get_mut(&item::<K>(args, 0)?).map(|value| {
                *value += delta;
                *value
            }))
        }
        "index" => json!(map[&item::<K>(args, 0)?]),
        "first_key_value" => optional_pair(map.first_key_value()),
        "last_key_value" => optional_pair(map.last_key_value()),
        "pop_first" => map.pop_first().map_or(Value::Null, |(key, value)| pair_json(&key, &value)),
        "pop_last" => map.pop_last().map_or(Value::Null, |(key, value)| pair_json(&key, &value)),
        "first_entry_add" => {
            let delta = number(args, 0)?;
            map.first_entry().map_or(Value::Null, |mut entry| {
                *entry.get_mut() += delta;
                pair_json(entry.key(), entry.get())
            })
        }
        "last_entry_add" => {
            let delta = number(args, 0)?;
            map.last_entry().map_or(Value::Null, |mut entry| {
                *entry.get_mut() += delta;
                pair_json(entry.key(), entry.get())
            })
        }
        "first_entry_remove" => map.first_entry().map_or(Value::Null, |entry| {
            let (key, value) = entry.remove_entry();
            pair_json(&key, &value)
        }),
        "last_entry_remove" => map.last_entry().map_or(Value::Null, |entry| json!(entry.remove())),
        "entry_or_insert" => {
            let value = map.entry(item(args, 0)?).or_insert(number(args, 1)?);
            *value += 1;
            json!(*value)
        }
        "entry_or_insert_with" => {
            let fallback = number(args, 1)?;
            let mut called = false;
            let value = map.entry(item(args, 0)?).or_insert_with(|| {
                called = true;
                fallback
            });
            *value += 1;
            json!([*value, called])
        }
        "entry_or_insert_with_key" => {
            json!(*map.entry(item(args, 0)?).or_insert_with_key(|key| key.derived()))
        }
        "entry_or_default" => {
            let value = map.entry(item(args, 0)?).or_default();
            *value += 1;
            json!(*value)
        }
        "entry_and_modify" => {
            let delta = number(args, 1)?;
            json!(*map
                .entry(item(args, 0)?)
                .and_modify(|value| *value += delta)
                .or_insert(number(args, 2)?))
        }
        "entry_key" => {
            let entry = map.entry(item(args, 0)?);
            let occupied = matches!(entry, Entry::Occupied(_));
            json!([entry.key().json(), occupied])
        }
        "entry_insert_entry" => {
            let entry = map.entry(item(args, 0)?).insert_entry(number(args, 1)?);
            pair_json(entry.key(), entry.get())
        }
        "entry_match" => {
            let mode = args.get(1)?.as_str()?;
            let value = number(args, 2)?;
            match map.entry(item(args, 0)?) {
                Entry::Occupied(mut entry) => match mode {
                    "get" => json!(["o", entry.key().json(), *entry.get()]),
                    "insert" => json!(["o", entry.insert(value)]),
                    "remove" => json!(["o", entry.remove()]),
                    "remove_entry" => {
                        let (key, value) = entry.remove_entry();
                        json!(["o", key.json(), value])
                    }
                    "into_mut" => {
                        let slot = entry.into_mut();
                        *slot += value;
                        json!(["o", *slot])
                    }
                    _ => return None,
                },
                Entry::Vacant(entry) => match mode {
                    "get" | "remove" | "remove_entry" => json!(["v", entry.key().json()]),
                    "insert" => json!(["v", *entry.insert(value)]),
                    "into_mut" => json!(["v", entry.into_key().json()]),
                    _ => return None,
                },
            }
        }
        "range" => pair_list(map.range((bound::<K>(args, 0)?, bound::<K>(args, 1)?))),
        "range_rev" => pair_list(map.range((bound::<K>(args, 0)?, bound::<K>(args, 1)?)).rev()),
        "range_mut_add" => {
            let delta = number(args, 2)?;
            let mut count = 0;
            for (_, value) in map.range_mut((bound::<K>(args, 0)?, bound::<K>(args, 1)?)) {
                *value += delta;
                count += 1;
            }
            json!(count)
        }
        "retain" => {
            let (modulus, rest) = (number(args, 0)?, number(args, 1)?);
            map.retain(|key, _| key.class(modulus) != rest);
            Value::Null
        }
        "retain_mut" => {
            let (modulus, rest, delta) = (number(args, 0)?, number(args, 1)?, number(args, 2)?);
            map.retain(|key, value| {
                *value += delta;
                key.class(modulus) != rest
            });
            Value::Null
        }
        "extract_if" => {
            let (modulus, rest) = (number(args, 2)?, number(args, 3)?);
            let take = args.get(4)?.as_u64().map_or(usize::MAX, |count| count as usize);
            let extracted: Vec<(K, i64)> = map
                .extract_if((bound::<K>(args, 0)?, bound::<K>(args, 1)?), |key, value| {
                    *value += 1;
                    key.class(modulus) == rest
                })
                .take(take)
                .collect();
            pair_list(extracted.iter().map(|(key, value)| (key, value)))
        }
        "split_off" => {
            let other = map.split_off(&item::<K>(args, 0)?);
            json!([pair_list(other.iter()), map_probe(&other)])
        }
        "append" => {
            let mut other: BTreeMap<K, i64> = pairs(args, 0)?.into_iter().collect();
            map.append(&mut other);
            json!([other.len(), map_probe(&other)])
        }
        "extend" => {
            map.extend(pairs::<K>(args, 0)?);
            Value::Null
        }
        "from_iter" => {
            *map = pairs(args, 0)?.into_iter().collect();
            Value::Null
        }
        "clear" => {
            map.clear();
            Value::Null
        }
        "clone" => {
            *map = map.clone();
            Value::Null
        }
        "take" => {
            let taken = std::mem::take(map);
            json!(taken.len())
        }
        "iter_rev" => pair_list(map.iter().rev()),
        "keys" => item_list(map.keys()),
        "keys_rev" => item_list(map.keys().rev()),
        "values" => json!(map.values().collect::<Vec<_>>()),
        "values_rev" => json!(map.values().rev().collect::<Vec<_>>()),
        "values_mut_add" => {
            let delta = number(args, 0)?;
            for value in map.values_mut() {
                *value += delta;
            }
            Value::Null
        }
        "len" => json!([map.len(), map.is_empty()]),
        "cmp" => {
            let other: BTreeMap<K, i64> = pairs(args, 0)?.into_iter().collect();
            json!([*map == other, ordering((*map).cmp(&other))])
        }
        "probe" => json!(map_probe(map)),
        _ => return None,
    })
}

fn btree_set_step<T: Item>(set: &mut BTreeSet<T>, name: &str, args: &[Value]) -> Option<Value> {
    let other = |at: usize| -> Option<BTreeSet<T>> { Some(items::<T>(args, at)?.into_iter().collect()) };
    Some(match name {
        "insert" => json!(set.insert(item(args, 0)?)),
        "replace" => set.replace(item(args, 0)?).map_or(Value::Null, |old| old.json()),
        "remove" => json!(set.remove(&item::<T>(args, 0)?)),
        "take" => set.take(&item::<T>(args, 0)?).map_or(Value::Null, |old| old.json()),
        "contains" => json!(set.contains(&item::<T>(args, 0)?)),
        "get" => optional_item(set.get(&item::<T>(args, 0)?)),
        "first" => optional_item(set.first()),
        "last" => optional_item(set.last()),
        "pop_first" => set.pop_first().map_or(Value::Null, |element| element.json()),
        "pop_last" => set.pop_last().map_or(Value::Null, |element| element.json()),
        "range" => item_list(set.range((bound::<T>(args, 0)?, bound::<T>(args, 1)?))),
        "range_rev" => item_list(set.range((bound::<T>(args, 0)?, bound::<T>(args, 1)?)).rev()),
        "retain" => {
            let (modulus, rest) = (number(args, 0)?, number(args, 1)?);
            set.retain(|element| element.class(modulus) != rest);
            Value::Null
        }
        "extract_if" => {
            let (modulus, rest) = (number(args, 2)?, number(args, 3)?);
            let take = args.get(4)?.as_u64().map_or(usize::MAX, |count| count as usize);
            let extracted: Vec<T> = set
                .extract_if((bound::<T>(args, 0)?, bound::<T>(args, 1)?), |element| {
                    element.class(modulus) == rest
                })
                .take(take)
                .collect();
            item_list(extracted.iter())
        }
        "split_off" => {
            let other = set.split_off(&item::<T>(args, 0)?);
            json!([item_list(other.iter()), set_probe(&other)])
        }
        "append" => {
            let mut other = other(0)?;
            set.append(&mut other);
            json!([other.len(), set_probe(&other)])
        }
        "extend" => {
            set.extend(items::<T>(args, 0)?);
            Value::Null
        }
        "from_iter" => {
            *set = other(0)?;
            Value::Null
        }
        "clear" => {
            set.clear();
            Value::Null
        }
        "clone" => {
            *set = set.clone();
            Value::Null
        }
        "iter_rev" => item_list(set.iter().rev()),
        "len" => json!([set.len(), set.is_empty()]),
        "cmp" => {
            let other = other(0)?;
            json!([*set == other, ordering((*set).cmp(&other))])
        }
        "probe" => json!(set_probe(set)),
        "union" => item_list(set.union(&other(0)?)),
        "intersection" => item_list(set.intersection(&other(0)?)),
        "difference" => item_list(set.difference(&other(0)?)),
        "symmetric_difference" => item_list(set.symmetric_difference(&other(0)?)),
        "is_subset" => json!(set.is_subset(&other(0)?)),
        "is_superset" => json!(set.is_superset(&other(0)?)),
        "is_disjoint" => json!(set.is_disjoint(&other(0)?)),
        "bitor" => {
            let result = &*set | &other(0)?;
            json!([item_list(result.iter()), set_probe(&result)])
        }
        "bitand" => {
            let result = &*set & &other(0)?;
            json!([item_list(result.iter()), set_probe(&result)])
        }
        "bitxor" => {
            let result = &*set ^ &other(0)?;
            json!([item_list(result.iter()), set_probe(&result)])
        }
        "sub" => {
            let result = &*set - &other(0)?;
            json!([item_list(result.iter()), set_probe(&result)])
        }
        _ => return None,
    })
}

fn sorted_pairs<K: Item>(entries: impl Iterator<Item = (K, i64)>) -> Value {
    let sorted: BTreeMap<K, i64> = entries.collect();
    pair_list(sorted.iter())
}

fn sorted_items<T: Item>(elements: impl Iterator<Item = T>) -> Value {
    let sorted: BTreeSet<T> = elements.collect();
    item_list(sorted.iter())
}

fn hash_map_step<K: Item>(map: &mut HashMap<K, i64>, name: &str, args: &[Value]) -> Option<Value> {
    use std::collections::hash_map::Entry;
    Some(match name {
        "insert" => json!(map.insert(item(args, 0)?, number(args, 1)?)),
        "remove" => json!(map.remove(&item::<K>(args, 0)?)),
        "remove_entry" => map
            .remove_entry(&item::<K>(args, 0)?)
            .map_or(Value::Null, |(key, value)| pair_json(&key, &value)),
        "get" => json!(map.get(&item::<K>(args, 0)?)),
        "get_key_value" => optional_pair(map.get_key_value(&item::<K>(args, 0)?)),
        "contains_key" => json!(map.contains_key(&item::<K>(args, 0)?)),
        "get_mut_add" => {
            let delta = number(args, 1)?;
            json!(map.get_mut(&item::<K>(args, 0)?).map(|value| {
                *value += delta;
                *value
            }))
        }
        "index" => json!(map[&item::<K>(args, 0)?]),
        "entry_or_insert" => {
            let value = map.entry(item(args, 0)?).or_insert(number(args, 1)?);
            *value += 1;
            json!(*value)
        }
        "entry_or_insert_with" => {
            let fallback = number(args, 1)?;
            let mut called = false;
            let value = map.entry(item(args, 0)?).or_insert_with(|| {
                called = true;
                fallback
            });
            *value += 1;
            json!([*value, called])
        }
        "entry_or_insert_with_key" => {
            json!(*map.entry(item(args, 0)?).or_insert_with_key(|key| key.derived()))
        }
        "entry_or_default" => {
            let value = map.entry(item(args, 0)?).or_default();
            *value += 1;
            json!(*value)
        }
        "entry_and_modify" => {
            let delta = number(args, 1)?;
            json!(*map
                .entry(item(args, 0)?)
                .and_modify(|value| *value += delta)
                .or_insert(number(args, 2)?))
        }
        "entry_key" => {
            let entry = map.entry(item(args, 0)?);
            let occupied = matches!(entry, Entry::Occupied(_));
            json!([entry.key().json(), occupied])
        }
        "entry_insert_entry" => {
            let entry = map.entry(item(args, 0)?).insert_entry(number(args, 1)?);
            pair_json(entry.key(), entry.get())
        }
        "entry_match" => {
            let mode = args.get(1)?.as_str()?;
            let value = number(args, 2)?;
            match map.entry(item(args, 0)?) {
                Entry::Occupied(mut entry) => match mode {
                    "get" => json!(["o", entry.key().json(), *entry.get()]),
                    "insert" => json!(["o", entry.insert(value)]),
                    "remove" => json!(["o", entry.remove()]),
                    "remove_entry" => {
                        let (key, value) = entry.remove_entry();
                        json!(["o", key.json(), value])
                    }
                    "into_mut" => {
                        let slot = entry.into_mut();
                        *slot += value;
                        json!(["o", *slot])
                    }
                    _ => return None,
                },
                Entry::Vacant(entry) => match mode {
                    "get" | "remove" | "remove_entry" => json!(["v", entry.key().json()]),
                    "insert" => json!(["v", *entry.insert(value)]),
                    "into_mut" => json!(["v", entry.into_key().json()]),
                    _ => return None,
                },
            }
        }
        "retain" => {
            let (modulus, rest) = (number(args, 0)?, number(args, 1)?);
            map.retain(|key, _| key.class(modulus) != rest);
            Value::Null
        }
        "retain_mut" => {
            let (modulus, rest, delta) = (number(args, 0)?, number(args, 1)?, number(args, 2)?);
            map.retain(|key, value| {
                *value += delta;
                key.class(modulus) != rest
            });
            Value::Null
        }
        "extract_if" => {
            let (modulus, rest) = (number(args, 0)?, number(args, 1)?);
            sorted_pairs(map.extract_if(|key, _| key.class(modulus) == rest))
        }
        "drain" => sorted_pairs(map.drain()),
        "extend" => {
            map.extend(pairs::<K>(args, 0)?);
            Value::Null
        }
        "from_iter" => {
            *map = pairs(args, 0)?.into_iter().collect();
            Value::Null
        }
        "with_capacity" => {
            *map = HashMap::with_capacity(index(args, 0)?);
            Value::Null
        }
        "clear" => {
            map.clear();
            Value::Null
        }
        "clone" => {
            *map = map.clone();
            Value::Null
        }
        "keys" => sorted_items(map.keys().cloned()),
        "values" => {
            let mut values: Vec<i64> = map.values().copied().collect();
            values.sort_unstable();
            json!(values)
        }
        "values_mut_add" => {
            let delta = number(args, 0)?;
            for value in map.values_mut() {
                *value += delta;
            }
            Value::Null
        }
        "iter_mut_add" => {
            let delta = number(args, 0)?;
            for (key, value) in map.iter_mut() {
                *value += delta + key.class(3);
            }
            Value::Null
        }
        "len" => json!([map.len(), map.is_empty()]),
        "eq" => {
            let other: HashMap<K, i64> = pairs(args, 0)?.into_iter().collect();
            json!(*map == other)
        }
        _ => return None,
    })
}

fn hash_set_step<T: Item>(set: &mut HashSet<T>, name: &str, args: &[Value]) -> Option<Value> {
    let other = |at: usize| -> Option<HashSet<T>> { Some(items::<T>(args, at)?.into_iter().collect()) };
    Some(match name {
        "insert" => json!(set.insert(item(args, 0)?)),
        "replace" => set.replace(item(args, 0)?).map_or(Value::Null, |old| old.json()),
        "remove" => json!(set.remove(&item::<T>(args, 0)?)),
        "take" => set.take(&item::<T>(args, 0)?).map_or(Value::Null, |old| old.json()),
        "contains" => json!(set.contains(&item::<T>(args, 0)?)),
        "get" => optional_item(set.get(&item::<T>(args, 0)?)),
        "retain" => {
            let (modulus, rest) = (number(args, 0)?, number(args, 1)?);
            set.retain(|element| element.class(modulus) != rest);
            Value::Null
        }
        "extract_if" => {
            let (modulus, rest) = (number(args, 0)?, number(args, 1)?);
            sorted_items(set.extract_if(|element| element.class(modulus) == rest))
        }
        "drain" => sorted_items(set.drain()),
        "extend" => {
            set.extend(items::<T>(args, 0)?);
            Value::Null
        }
        "from_iter" => {
            *set = other(0)?;
            Value::Null
        }
        "clear" => {
            set.clear();
            Value::Null
        }
        "clone" => {
            *set = set.clone();
            Value::Null
        }
        "len" => json!([set.len(), set.is_empty()]),
        "eq" => json!(*set == other(0)?),
        "union" => sorted_items(set.union(&other(0)?).cloned()),
        "intersection" => sorted_items(set.intersection(&other(0)?).cloned()),
        "difference" => sorted_items(set.difference(&other(0)?).cloned()),
        "symmetric_difference" => sorted_items(set.symmetric_difference(&other(0)?).cloned()),
        "is_subset" => json!(set.is_subset(&other(0)?)),
        "is_superset" => json!(set.is_superset(&other(0)?)),
        "is_disjoint" => json!(set.is_disjoint(&other(0)?)),
        "bitor" => sorted_items((&*set | &other(0)?).into_iter()),
        "bitand" => sorted_items((&*set & &other(0)?).into_iter()),
        "bitxor" => sorted_items((&*set ^ &other(0)?).into_iter()),
        "sub" => sorted_items((&*set - &other(0)?).into_iter()),
        _ => return None,
    })
}

/// The state of a deque is its two slices and its capacity: the port keeps
/// the same buffer layout as Rust, and this is how that is checked.
fn vec_deque_state<T: Item>(deque: &VecDeque<T>, full: bool) -> Value {
    let (front, back) = deque.as_slices();
    if full {
        return json!([item_list(front.iter()), item_list(back.iter()), deque.capacity()]);
    }
    let mut digest = Fnv::new();
    for element in deque {
        element.feed(&mut digest);
    }
    json!([front.len(), back.len(), deque.capacity(), digest.text()])
}

/// A deque with the given elements whose storage wraps around after
/// `rotation` of them, when the rotation is not zero.
fn rotated<T: Item>(elements: Vec<T>, rotation: usize) -> VecDeque<T> {
    let mut deque = VecDeque::with_capacity(elements.len());
    let split = if elements.is_empty() { 0 } else { rotation % elements.len() };
    for element in &elements[split..] {
        deque.push_back(element.clone());
    }
    for element in elements[..split].iter().rev() {
        deque.push_front(element.clone());
    }
    deque
}

fn vec_deque_step<T: Item>(deque: &mut VecDeque<T>, name: &str, args: &[Value]) -> Option<Value> {
    let optional = |found: Option<T>| found.map_or(Value::Null, |element| element.json());
    Some(match name {
        "push_back" => {
            deque.push_back(item(args, 0)?);
            Value::Null
        }
        "push_front" => {
            deque.push_front(item(args, 0)?);
            Value::Null
        }
        "push_back_mut_add" => {
            let slot = deque.push_back_mut(item(args, 0)?);
            slot.add(number(args, 1)?);
            slot.json()
        }
        "push_front_mut_add" => {
            let slot = deque.push_front_mut(item(args, 0)?);
            slot.add(number(args, 1)?);
            slot.json()
        }
        "pop_back" => optional(deque.pop_back()),
        "pop_front" => optional(deque.pop_front()),
        "pop_back_if" => {
            let limit = number(args, 0)?;
            optional(deque.pop_back_if(|element| element.number() < limit))
        }
        "pop_front_if" => {
            let limit = number(args, 0)?;
            optional(deque.pop_front_if(|element| element.number() < limit))
        }
        "insert" => {
            deque.insert(index(args, 0)?, item(args, 1)?);
            Value::Null
        }
        "insert_mut_add" => {
            let slot = deque.insert_mut(index(args, 0)?, item(args, 1)?);
            slot.add(number(args, 2)?);
            slot.json()
        }
        "remove" => optional(deque.remove(index(args, 0)?)),
        "swap" => {
            deque.swap(index(args, 0)?, index(args, 1)?);
            Value::Null
        }
        "swap_remove_back" => optional(deque.swap_remove_back(index(args, 0)?)),
        "swap_remove_front" => optional(deque.swap_remove_front(index(args, 0)?)),
        "truncate" => {
            deque.truncate(index(args, 0)?);
            Value::Null
        }
        "retain_back" => {
            deque.retain_back(index(args, 0)?);
            Value::Null
        }
        "clear" => {
            deque.clear();
            Value::Null
        }
        "reserve" => {
            deque.reserve(index(args, 0)?);
            Value::Null
        }
        "reserve_exact" => {
            deque.reserve_exact(index(args, 0)?);
            Value::Null
        }
        "shrink_to_fit" => {
            deque.shrink_to_fit();
            Value::Null
        }
        "shrink_to" => {
            deque.shrink_to(index(args, 0)?);
            Value::Null
        }
        "rotate_left" => {
            deque.rotate_left(index(args, 0)?);
            Value::Null
        }
        "rotate_right" => {
            deque.rotate_right(index(args, 0)?);
            Value::Null
        }
        "make_contiguous" => item_list(deque.make_contiguous().iter()),
        "drain" => {
            let drained: Vec<T> = deque.drain((index_bound(args, 0)?, index_bound(args, 1)?)).collect();
            item_list(drained.iter())
        }
        "drain_rev" => {
            let drained: Vec<T> =
                deque.drain((index_bound(args, 0)?, index_bound(args, 1)?)).rev().collect();
            item_list(drained.iter())
        }
        "split_off" => {
            let other = deque.split_off(index(args, 0)?);
            vec_deque_state(&other, true)
        }
        "append" => {
            let mut other = rotated(items::<T>(args, 0)?, index(args, 1)?);
            deque.append(&mut other);
            vec_deque_state(&other, true)
        }
        "extend" => {
            // A filter hides the length, as a Go sequence does.
            let elements = items::<T>(args, 0)?;
            deque.extend(elements.iter().cloned().filter(|_| true));
            Value::Null
        }
        "resize" => {
            deque.resize(index(args, 0)?, item(args, 1)?);
            Value::Null
        }
        "resize_with" => {
            let mut next: T = item(args, 1)?;
            deque.resize_with(index(args, 0)?, || {
                let value = next.clone();
                next.add(1);
                value
            });
            Value::Null
        }
        "retain" => {
            let (modulus, rest) = (number(args, 0)?, number(args, 1)?);
            deque.retain(|element| element.class(modulus) != rest);
            Value::Null
        }
        "retain_mut" => {
            let (modulus, rest, delta) = (number(args, 0)?, number(args, 1)?, number(args, 2)?);
            deque.retain_mut(|element| {
                let keep = element.class(modulus) != rest;
                element.add(delta);
                keep
            });
            Value::Null
        }
        "range" => item_list(deque.range((index_bound(args, 0)?, index_bound(args, 1)?))),
        "range_rev" => item_list(deque.range((index_bound(args, 0)?, index_bound(args, 1)?)).rev()),
        "range_mut_add" => {
            let delta = number(args, 2)?;
            let mut count = 0;
            for element in deque.range_mut((index_bound(args, 0)?, index_bound(args, 1)?)) {
                element.add(delta);
                count += 1;
            }
            json!(count)
        }
        "get" => optional_item(deque.get(index(args, 0)?)),
        "get_mut_add" => {
            let delta = number(args, 1)?;
            deque.get_mut(index(args, 0)?).map_or(Value::Null, |element| {
                element.add(delta);
                element.json()
            })
        }
        "front" => optional_item(deque.front()),
        "back" => optional_item(deque.back()),
        "front_mut_add" => {
            let delta = number(args, 0)?;
            deque.front_mut().map_or(Value::Null, |element| {
                element.add(delta);
                element.json()
            })
        }
        "back_mut_add" => {
            let delta = number(args, 0)?;
            deque.back_mut().map_or(Value::Null, |element| {
                element.add(delta);
                element.json()
            })
        }
        "index" => deque[index(args, 0)?].json(),
        "index_mut_add" => {
            let slot = &mut deque[index(args, 0)?];
            slot.add(number(args, 1)?);
            slot.json()
        }
        "contains" => json!(deque.contains(&item::<T>(args, 0)?)),
        "binary_search" => match deque.binary_search(&item::<T>(args, 0)?) {
            Ok(at) => json!([at, true]),
            Err(at) => json!([at, false]),
        },
        "binary_search_by_key" => {
            let key = number(args, 0)?;
            match deque.binary_search_by_key(&key, |element| element.number() / 2) {
                Ok(at) => json!([at, true]),
                Err(at) => json!([at, false]),
            }
        }
        "partition_point" => {
            let limit = number(args, 0)?;
            json!(deque.partition_point(|element| element.number() < limit))
        }
        "iter" => item_list(deque.iter()),
        "iter_rev" => item_list(deque.iter().rev()),
        "iter_mut_add" => {
            let delta = number(args, 0)?;
            for element in deque.iter_mut() {
                element.add(delta);
            }
            Value::Null
        }
        "len" => json!([deque.len(), deque.is_empty()]),
        "eq" => json!(*deque == items::<T>(args, 0)?),
        "eq_deque" => json!(*deque == rotated(items::<T>(args, 0)?, index(args, 1)?)),
        "cmp" => json!(ordering((*deque).cmp(&rotated(items::<T>(args, 0)?, index(args, 1)?)))),
        "from" => {
            *deque = VecDeque::from(items::<T>(args, 0)?);
            // A Vec built from a JSON array may hold spare capacity, which a
            // slice handed to the port does not carry.
            deque.shrink_to_fit();
            Value::Null
        }
        "from_iter" => {
            // Over a borrowed slice, so that the Vec the deque is collected
            // through is built by pushing and not in place.
            let elements = items::<T>(args, 0)?;
            *deque = elements.iter().cloned().filter(|_| true).collect();
            Value::Null
        }
        "with_capacity" => {
            *deque = VecDeque::with_capacity(index(args, 0)?);
            Value::Null
        }
        "new" => {
            *deque = VecDeque::new();
            Value::Null
        }
        "clone" => {
            *deque = deque.clone();
            Value::Null
        }
        "into_vec" => {
            let vector: Vec<T> = Vec::from(std::mem::take(deque));
            let listed = item_list(vector.iter());
            listed
        }
        _ => return None,
    })
}

/// `DefaultHasher` fed one write at a time; the reply is `finish()` after
/// each write, as decimal text.
fn sip(request: &Value) -> Option<Value> {
    let mut hasher = std::collections::hash_map::DefaultHasher::new();
    let mut finishes = vec![hasher.finish().to_string()];
    for write in request.get("writes")?.as_array()? {
        let write = write.as_array()?;
        let value = write.get(1)?;
        match write.first()?.as_str()? {
            "bytes" => {
                let bytes: Vec<u8> =
                    value.as_array()?.iter().map(|byte| byte.as_u64().map(|b| b as u8)).collect::<Option<_>>()?;
                hasher.write(&bytes);
            }
            // `write_str` and `write_length_prefix` are not callable from
            // stable code; the Hash impls of `str` and of slices call them.
            "str" => value.as_str()?.hash(&mut hasher),
            "u8" => hasher.write_u8(value.as_u64()? as u8),
            "u16" => hasher.write_u16(value.as_u64()? as u16),
            "u32" => hasher.write_u32(value.as_u64()? as u32),
            "u64" => hasher.write_u64(value.as_str()?.parse().ok()?),
            "usize" => hasher.write_usize(value.as_u64()? as usize),
            "i8" => hasher.write_i8(value.as_i64()? as i8),
            "i16" => hasher.write_i16(value.as_i64()? as i16),
            "i32" => hasher.write_i32(value.as_i64()? as i32),
            "i64" => hasher.write_i64(value.as_i64()?),
            "isize" => hasher.write_isize(value.as_i64()? as isize),
            "length_prefix" => vec![(); value.as_u64()? as usize][..].hash(&mut hasher),
            _ => return None,
        }
        finishes.push(hasher.finish().to_string());
    }
    Some(json!({ "finish": finishes }))
}

#[derive(Hash)]
#[allow(dead_code)]
enum Shape {
    Unit,
    Tuple(u32, String),
    Named { wide: u64, flag: bool },
}

#[derive(Hash)]
struct Record {
    name: String,
    count: u16,
    tags: Vec<char>,
    shape: Shape,
    maybe: Option<i32>,
}

/// The hashes of a few fixed values, which the Go test rebuilds from the
/// hasher's write methods as the documentation of `HashMapDefaultHasher`
/// describes.
fn hash_value() -> Value {
    fn hashed<T: Hash + ?Sized>(value: &T) -> String {
        let mut hasher = std::collections::hash_map::DefaultHasher::new();
        value.hash(&mut hasher);
        hasher.finish().to_string()
    }
    let window: Vec<char> = "héllo, wörld".chars().collect();
    json!({
        "str": hashed(&"session-1"),
        "string": hashed(&"session-1".to_owned()),
        "chars": hashed(&window[..]),
        "tuple": hashed(&("a", 7u8, -3i64, true)),
        "option_some": hashed(&Some(9u32)),
        "option_none": hashed(&None::<u32>),
        "unit_variant": hashed(&Shape::Unit),
        "tuple_variant": hashed(&Shape::Tuple(5, "x".to_owned())),
        "named_variant": hashed(&Shape::Named { wide: 1 << 40, flag: true }),
        "record": hashed(&Record {
            name: "n".to_owned(),
            count: 513,
            tags: vec!['a', 'ß'],
            shape: Shape::Tuple(1, String::new()),
            maybe: Some(-1),
        }),
        "bytes": hashed(&[1u8, 2, 3][..]),
        "u16s": hashed(&[1u16, 2, 3][..]),
        "usize": hashed(&77usize),
        "char": hashed(&'é'),
        "bool": hashed(&false),
        "unit": hashed(&()),
        "btree_map": hashed(&BTreeMap::from([(1u8, "a"), (2u8, "b")])),
        "vec_deque": hashed(&VecDeque::from([3u32, 4])),
    })
}
