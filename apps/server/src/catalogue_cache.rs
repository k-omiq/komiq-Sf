//! Bounded catalogue values with refill generations and selective LRU eviction.
//! A generation captured before an async read cannot publish after invalidation.
use std::borrow::Borrow;
use std::collections::HashMap;
use std::hash::Hash;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use crate::telemetry::CacheStats;

#[derive(Clone, Default)]
pub(crate) struct CacheEpoch(Arc<AtomicU64>);

impl CacheEpoch {
    pub(crate) fn current(&self) -> u64 {
        self.0.load(Ordering::Acquire)
    }

    pub(crate) fn advance(&self) {
        self.0.fetch_add(1, Ordering::AcqRel);
    }
}

struct Entry<V> {
    value: V,
    stored: Instant,
    generation: u64,
    used: u64,
}

struct Values<K, V> {
    entries: HashMap<K, Entry<V>>,
    clock: u64,
}

pub(crate) struct CatalogueCache<K, V> {
    capacity: usize,
    epoch: CacheEpoch,
    values: Mutex<Values<K, V>>,
    stats: &'static CacheStats,
}

impl<K: Eq + Hash + Clone, V: Clone> CatalogueCache<K, V> {
    pub(crate) fn new(capacity: usize, epoch: CacheEpoch, stats: &'static CacheStats) -> Self {
        Self {
            capacity,
            epoch,
            stats,
            values: Mutex::new(Values {
                entries: HashMap::new(),
                clock: 0,
            }),
        }
    }

    #[cfg(test)]
    pub(crate) fn generation(&self) -> u64 {
        self.epoch.current()
    }

    pub(crate) fn get<Q: Eq + Hash + ?Sized>(&self, key: &Q, ttl: Duration) -> Option<V>
    where
        K: Borrow<Q>,
    {
        let mut values = self.values.lock().ok()?;
        let generation = self.epoch.current();
        let fresh = values
            .entries
            .get(key)
            .is_some_and(|entry| entry.generation == generation && entry.stored.elapsed() < ttl);
        if !fresh {
            if values.entries.remove(key).is_some() {
                self.stats.expired(1);
                self.stats.set_entries(values.entries.len());
            }
            self.stats.misses(1);
            return None;
        }
        values.clock = values.clock.wrapping_add(1);
        let used = values.clock;
        let entry = values.entries.get_mut(key)?;
        entry.used = used;
        self.stats.hits(1);
        Some(entry.value.clone())
    }

    pub(crate) fn insert(&self, generation: u64, key: K, value: V, ttl: Duration) -> bool {
        let Ok(mut values) = self.values.lock() else {
            return false;
        };
        if self.capacity == 0 {
            return false;
        }
        if generation != self.epoch.current() {
            self.stats.stale_refills_rejected(1);
            return false;
        }
        if !values.entries.contains_key(&key) && values.entries.len() >= self.capacity {
            let before = values.entries.len();
            values
                .entries
                .retain(|_, entry| entry.generation == generation && entry.stored.elapsed() < ttl);
            self.stats.expired((before - values.entries.len()) as u64);
            if values.entries.len() >= self.capacity {
                if let Some(oldest) = values
                    .entries
                    .iter()
                    .min_by_key(|(_, entry)| entry.used)
                    .map(|(key, _)| key.clone())
                {
                    values.entries.remove(&oldest);
                    self.stats.evictions(1);
                }
            }
        }
        values.clock = values.clock.wrapping_add(1);
        let used = values.clock;
        values.entries.insert(
            key,
            Entry {
                value,
                stored: Instant::now(),
                generation,
                used,
            },
        );
        self.stats.fills(1);
        self.stats.set_entries(values.entries.len());
        // If invalidation raced the insert, get() rejects its old stamp even before clear().
        true
    }

    pub(crate) fn clear(&self) {
        if let Ok(mut values) = self.values.lock() {
            self.stats.invalidated(values.entries.len() as u64);
            values.entries.clear();
            self.stats.set_entries(0);
        }
    }

    #[cfg(test)]
    pub(crate) fn len(&self) -> usize {
        self.values.lock().unwrap().entries.len()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn live_key_flood_keeps_recently_read_values_and_never_clears_the_cache() {
        let epoch = CacheEpoch::default();
        let cache = CatalogueCache::new(3, epoch.clone(), CacheStats::leak());
        let ttl = Duration::from_secs(60);
        cache.insert(epoch.current(), "hot".to_string(), 42, ttl);
        for i in 0..100 {
            assert_eq!(cache.get("hot", ttl), Some(42));
            cache.insert(epoch.current(), format!("cold-{i}"), i, ttl);
            assert_eq!(cache.len(), 3.min(i as usize + 2));
        }
        assert_eq!(cache.get("hot", ttl), Some(42));
        cache.insert(epoch.current(), "hot".to_string(), 43, ttl);
        assert_eq!(
            cache.len(),
            3,
            "updating an existing key must not evict unrelated entries"
        );
        assert_eq!(cache.get("hot", ttl), Some(43));
    }

    #[tokio::test]
    async fn invalidation_rejects_an_already_running_refill_across_shared_caches() {
        let epoch = CacheEpoch::default();
        let stats = CacheStats::leak();
        let counts = Arc::new(CatalogueCache::new(2, epoch.clone(), stats));
        let rows = CatalogueCache::new(2, epoch.clone(), CacheStats::leak());
        let ttl = Duration::from_secs(60);
        let before = epoch.current();
        counts.insert(before, "key", 1, ttl);
        rows.insert(before, "key", vec!["old"], ttl);
        let (ready, started) = tokio::sync::oneshot::channel();
        let (release, resumed) = tokio::sync::oneshot::channel();
        let worker = counts.clone();
        let refill = tokio::spawn(async move {
            ready.send(()).unwrap();
            resumed.await.unwrap();
            worker.insert(before, "key", 99, ttl)
        });
        started.await.unwrap();
        epoch.advance();
        assert_eq!(counts.get(&"key", ttl), None);
        assert_eq!(
            rows.get(&"key", ttl),
            None,
            "old values are hidden before physical clears"
        );
        counts.clear();
        rows.clear();
        counts.insert(epoch.current(), "key", 2, ttl);
        release.send(()).unwrap();
        assert!(!refill.await.unwrap());
        assert_eq!(counts.get(&"key", ttl), Some(2));
        let seen = stats.snapshot();
        assert_eq!(
            seen.stale_refills_rejected, 1,
            "the late refill was refused"
        );
        // Old-generation value seen once (expired), cleared none (already removed).
        assert_eq!((seen.expired, seen.invalidated), (1, 0));
        assert_eq!((seen.fills, seen.hits, seen.misses), (2, 1, 1));
        assert_eq!(seen.entries, 1);
    }

    #[test]
    fn telemetry_counts_hits_misses_capacity_evictions_and_invalidations() {
        let epoch = CacheEpoch::default();
        let stats = CacheStats::leak();
        let cache = CatalogueCache::new(2, epoch.clone(), stats);
        let ttl = Duration::from_secs(60);
        assert_eq!(cache.get("a", ttl), None);
        cache.insert(epoch.current(), "a", 1, ttl);
        cache.insert(epoch.current(), "b", 2, ttl);
        assert_eq!(cache.get("a", ttl), Some(1));
        cache.insert(epoch.current(), "c", 3, ttl);
        let seen = stats.snapshot();
        assert_eq!((seen.hits, seen.misses, seen.fills), (1, 1, 3));
        assert_eq!((seen.evictions, seen.expired, seen.entries), (1, 0, 2));
        cache.clear();
        let seen = stats.snapshot();
        assert_eq!((seen.invalidated, seen.entries), (2, 0));
    }

    #[test]
    fn expiry_does_not_extend_on_read_and_zero_capacity_does_not_store() {
        let epoch = CacheEpoch::default();
        let cache = CatalogueCache::new(1, epoch.clone(), CacheStats::leak());
        cache.insert(epoch.current(), "old", 1, Duration::ZERO);
        assert_eq!(cache.get(&"old", Duration::ZERO), None);
        let disabled = CatalogueCache::new(0, epoch.clone(), CacheStats::leak());
        assert!(!disabled.insert(epoch.current(), "key", 1, Duration::from_secs(60)));
        assert_eq!(disabled.len(), 0);
    }
}
