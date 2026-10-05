//! Immutable snapshots with copy-on-write updates to individual records.

use std::borrow::Borrow;
use std::ops::{Index, IndexMut};
use std::sync::Arc;

use serde::{Deserialize, Serialize};

/// Cloning shares the tree and its records. Mutating a record copies only that
/// record and its tree path, even while readers hold an older snapshot.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(transparent)]
pub struct SnapshotMap<K: Ord + Clone, V: Clone>(imbl::OrdMap<K, Arc<V>>);

impl<K: Ord + Clone, V: Clone> Default for SnapshotMap<K, V> {
    fn default() -> Self {
        Self(imbl::OrdMap::new())
    }
}

impl<K: Ord + Clone, V: Clone> SnapshotMap<K, V> {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn len(&self) -> usize {
        self.0.len()
    }

    pub fn is_empty(&self) -> bool {
        self.0.is_empty()
    }

    pub fn get<Q: Ord + ?Sized>(&self, key: &Q) -> Option<&V>
    where
        K: Borrow<Q>,
    {
        self.0.get(key).map(Arc::as_ref)
    }

    /// Retain one immutable record without copying its contents.
    pub fn get_shared<Q: Ord + ?Sized>(&self, key: &Q) -> Option<Arc<V>>
    where
        K: Borrow<Q>,
    {
        self.0.get(key).cloned()
    }

    pub fn get_mut<Q: Ord + ?Sized>(&mut self, key: &Q) -> Option<&mut V>
    where
        K: Borrow<Q>,
    {
        self.0.get_mut(key).map(Arc::make_mut)
    }

    pub fn contains_key<Q: Ord + ?Sized>(&self, key: &Q) -> bool
    where
        K: Borrow<Q>,
    {
        self.0.contains_key(key)
    }

    pub fn insert(&mut self, key: K, value: V) -> Option<V> {
        self.0
            .insert(key, Arc::new(value))
            .map(Arc::unwrap_or_clone)
    }

    /// Replace a record without copying the previous value for the caller.
    pub fn insert_shared(&mut self, key: K, value: V) -> Option<Arc<V>> {
        self.0.insert(key, Arc::new(value))
    }

    pub fn remove<Q: Ord + ?Sized>(&mut self, key: &Q) -> Option<V>
    where
        K: Borrow<Q>,
    {
        self.0.remove(key).map(Arc::unwrap_or_clone)
    }

    /// Remove a record while retaining its shared contents for existing readers.
    pub fn remove_shared<Q: Ord + ?Sized>(&mut self, key: &Q) -> Option<Arc<V>>
    where
        K: Borrow<Q>,
    {
        self.0.remove(key)
    }

    pub fn clear(&mut self) {
        self.0.clear();
    }

    pub fn iter(&self) -> impl DoubleEndedIterator<Item = (&K, &V)> + ExactSizeIterator {
        self.0.iter().map(|(key, value)| (key, value.as_ref()))
    }

    pub fn keys(&self) -> impl DoubleEndedIterator<Item = &K> + ExactSizeIterator {
        self.0.keys()
    }

    pub fn values(&self) -> impl DoubleEndedIterator<Item = &V> + ExactSizeIterator {
        self.0.values().map(Arc::as_ref)
    }

    /// Changes needed to turn this snapshot into `next`. Shared tree branches
    /// are skipped, so an unchanged history is not walked on each publication.
    pub fn changes<'a>(&'a self, next: &'a Self) -> impl Iterator<Item = (&'a K, Option<&'a V>)>
    where
        V: PartialEq,
    {
        self.0.diff(&next.0).map(|change| match change {
            imbl::ordmap::DiffItem::Add(key, value)
            | imbl::ordmap::DiffItem::Update {
                new: (key, value), ..
            } => (key, Some(value.as_ref())),
            imbl::ordmap::DiffItem::Remove(key, _) => (key, None),
        })
    }

    pub fn into_values(self) -> impl Iterator<Item = V> {
        self.0
            .into_iter()
            .map(|(_, value)| Arc::unwrap_or_clone(value))
    }

    pub fn first_value_mut(&mut self) -> Option<&mut V> {
        let key = self.0.keys().next()?.clone();
        self.get_mut(&key)
    }

    pub fn entry(&mut self, key: K) -> SnapshotEntry<'_, K, V> {
        SnapshotEntry { map: self, key }
    }

    pub fn retain(&mut self, mut keep: impl FnMut(&K, &V) -> bool) {
        let removed: Vec<_> = self
            .iter()
            .filter(|(k, v)| !keep(k, v))
            .map(|(k, _)| k.clone())
            .collect();
        for key in removed {
            self.0.remove(&key);
        }
    }
}

pub struct SnapshotEntry<'a, K: Ord + Clone, V: Clone> {
    map: &'a mut SnapshotMap<K, V>,
    key: K,
}

impl<'a, K: Ord + Clone, V: Clone> SnapshotEntry<'a, K, V> {
    pub fn or_insert_with(self, value: impl FnOnce() -> V) -> &'a mut V {
        if !self.map.contains_key(&self.key) {
            self.map.insert(self.key.clone(), value());
        }
        self.map.get_mut(&self.key).expect("entry was inserted")
    }
}

impl<K: Ord + Clone, V: Clone> FromIterator<(K, V)> for SnapshotMap<K, V> {
    fn from_iter<T: IntoIterator<Item = (K, V)>>(iter: T) -> Self {
        Self(iter.into_iter().map(|(k, v)| (k, Arc::new(v))).collect())
    }
}

impl<K: Ord + Clone, V: Clone> Extend<(K, V)> for SnapshotMap<K, V> {
    fn extend<T: IntoIterator<Item = (K, V)>>(&mut self, iter: T) {
        self.0
            .extend(iter.into_iter().map(|(k, v)| (k, Arc::new(v))));
    }
}

impl<K: Ord + Clone, V: Clone, const N: usize> From<[(K, V); N]> for SnapshotMap<K, V> {
    fn from(values: [(K, V); N]) -> Self {
        values.into_iter().collect()
    }
}

impl<K: Ord + Clone, V: Clone, Q: Ord + ?Sized> Index<&Q> for SnapshotMap<K, V>
where
    K: Borrow<Q>,
{
    type Output = V;
    fn index(&self, key: &Q) -> &V {
        self.get(key).expect("snapshot map key is absent")
    }
}

impl<K: Ord + Clone, V: Clone, Q: Ord + ?Sized> IndexMut<&Q> for SnapshotMap<K, V>
where
    K: Borrow<Q>,
{
    fn index_mut(&mut self, key: &Q) -> &mut V {
        self.get_mut(key).expect("snapshot map key is absent")
    }
}

impl<K: Ord + Clone, V: Clone> IntoIterator for SnapshotMap<K, V> {
    type Item = (K, V);
    type IntoIter = std::iter::Map<
        imbl::ordmap::ConsumingIter<K, Arc<V>, imbl::shared_ptr::DefaultSharedPtr>,
        fn((K, Arc<V>)) -> (K, V),
    >;
    fn into_iter(self) -> Self::IntoIter {
        self.0
            .into_iter()
            .map(|(k, v)| (k, Arc::unwrap_or_clone(v)))
    }
}

impl<'a, K: Ord + Clone, V: Clone> IntoIterator for &'a SnapshotMap<K, V> {
    type Item = (&'a K, &'a V);
    type IntoIter = std::iter::Map<
        imbl::ordmap::Iter<'a, K, Arc<V>, imbl::shared_ptr::DefaultSharedPtr>,
        fn((&'a K, &'a Arc<V>)) -> (&'a K, &'a V),
    >;
    fn into_iter(self) -> Self::IntoIter {
        self.0.iter().map(|(k, v)| (k, v.as_ref()))
    }
}
