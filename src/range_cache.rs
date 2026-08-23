use std::collections::HashMap;
use std::future::Future;
use std::sync::Arc;

use tokio::sync::RwLock;

use crate::db::{self, StoredRange};
use crate::error::Result;

const MAX_ENTRIES: usize = 4096;

/// The key for a cached contextual range: the player, the abstracted sequence
/// node, and the effective stack-depth bucket.
#[derive(Clone, Debug, Hash, PartialEq, Eq)]
pub struct RangeKey {
    pub profile_id: i32,
    pub node: String,
    pub stack_bucket: i16,
}

pub trait RangeStore {
    fn load_range(
        &self,
        key: &RangeKey,
    ) -> impl Future<Output = Result<Option<StoredRange>>> + Send;

    fn store_range(
        &self,
        key: &RangeKey,
        range: &StoredRange,
    ) -> impl Future<Output = Result<()>> + Send;
}

pub struct PgRangeStore {
    pool: sqlx::PgPool,
}

impl PgRangeStore {
    pub fn new(pool: sqlx::PgPool) -> Self {
        Self { pool }
    }
}

impl RangeStore for PgRangeStore {
    async fn load_range(&self, key: &RangeKey) -> Result<Option<StoredRange>> {
        db::load_contextual_range(&self.pool, key.profile_id, &key.node, key.stack_bucket).await
    }

    async fn store_range(&self, key: &RangeKey, range: &StoredRange) -> Result<()> {
        db::upsert_contextual_range(
            &self.pool,
            key.profile_id,
            &key.node,
            key.stack_bucket,
            range,
        )
        .await
    }
}

#[derive(Default)]
pub struct RangeCache {
    entries: RwLock<HashMap<RangeKey, Arc<StoredRange>>>,
}

impl RangeCache {
    pub fn new() -> Self {
        Self::default()
    }

    pub async fn get_or_load<S: RangeStore>(
        &self,
        store: &S,
        key: RangeKey,
    ) -> Result<Option<Arc<StoredRange>>> {
        if let Some(range) = self.entries.read().await.get(&key).cloned() {
            return Ok(Some(range));
        }

        let range = store.load_range(&key).await?;
        let arc = range.map(Arc::new);
        if let Some(cached) = arc.clone() {
            self.insert(key, cached).await;
        }
        Ok(arc)
    }

    pub async fn put<S>(&self, store: S, key: RangeKey, range: StoredRange)
    where
        S: RangeStore + Send + 'static,
    {
        let arc = Arc::new(range);
        self.insert(key.clone(), arc.clone()).await;

        tokio::spawn(async move {
            if let Err(e) = store.store_range(&key, &arc).await {
                tracing::warn!(
                    error = %e,
                    profile_id = key.profile_id,
                    node = %key.node,
                    stack_bucket = key.stack_bucket,
                    "range write-back to database failed"
                );
            }
        });
    }

    async fn insert(&self, key: RangeKey, range: Arc<StoredRange>) {
        let mut entries = self.entries.write().await;
        if entries.len() >= MAX_ENTRIES && !entries.contains_key(&key) {
            entries.clear();
        }
        entries.insert(key, range);
    }
}

#[cfg(test)]
mod tests {
    use std::collections::HashMap;
    use std::sync::Mutex;
    use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
    use std::time::Duration;

    use super::*;
    use crate::error::Error;
    use crate::range::hands::{HAND_COUNT, Range};

    /// The mock store's shared, locked entry table.
    type MockEntries = Arc<Mutex<HashMap<(i32, String, i16), StoredRange>>>;

    #[derive(Clone)]
    struct MockStore {
        data: MockEntries,
        failures: Arc<AtomicBool>,
        attempts: Arc<AtomicUsize>,
        writes_enabled: Arc<AtomicBool>,
    }

    impl Default for MockStore {
        fn default() -> Self {
            Self {
                data: Arc::new(Mutex::new(HashMap::new())),
                failures: Arc::new(AtomicBool::new(false)),
                attempts: Arc::new(AtomicUsize::new(0)),
                writes_enabled: Arc::new(AtomicBool::new(true)),
            }
        }
    }

    impl MockStore {
        fn preset(entries: HashMap<(i32, String, i16), StoredRange>) -> Self {
            Self {
                data: Arc::new(Mutex::new(entries)),
                ..Self::default()
            }
        }

        fn fail_always() -> Self {
            Self {
                failures: Arc::new(AtomicBool::new(true)),
                ..Self::default()
            }
        }

        fn noop_writes() -> Self {
            Self {
                writes_enabled: Arc::new(AtomicBool::new(false)),
                ..Self::default()
            }
        }

        fn keys(&self) -> Vec<(i32, String, i16)> {
            self.data.lock().unwrap().keys().cloned().collect()
        }
    }

    impl RangeStore for MockStore {
        async fn load_range(&self, key: &RangeKey) -> Result<Option<StoredRange>> {
            if self.failures.load(Ordering::SeqCst) {
                return Err(Error::Store("mock load failure".to_string()));
            }
            Ok(self
                .data
                .lock()
                .unwrap()
                .get(&(key.profile_id, key.node.clone(), key.stack_bucket))
                .cloned())
        }

        async fn store_range(&self, key: &RangeKey, range: &StoredRange) -> Result<()> {
            self.attempts.fetch_add(1, Ordering::SeqCst);
            if self.failures.load(Ordering::SeqCst) {
                return Err(Error::Store("mock store failure".to_string()));
            }
            if self.writes_enabled.load(Ordering::SeqCst) {
                self.data.lock().unwrap().insert(
                    (key.profile_id, key.node.clone(), key.stack_bucket),
                    range.clone(),
                );
            }
            Ok(())
        }
    }

    fn key(id: i32, node: &str, bucket: i16) -> RangeKey {
        RangeKey {
            profile_id: id,
            node: node.to_string(),
            stack_bucket: bucket,
        }
    }

    fn stored(fill: f32, sample_count: u32) -> StoredRange {
        StoredRange {
            weights: [fill; HAND_COUNT],
            sample_count,
        }
    }

    async fn wait_until(mut condition: impl FnMut() -> bool) {
        for _ in 0..500 {
            if condition() {
                return;
            }
            tokio::time::sleep(Duration::from_millis(2)).await;
        }
        panic!("condition not reached in time");
    }

    #[tokio::test]
    async fn get_or_load_returns_none_for_missing_range() {
        let cache = RangeCache::new();
        let store = MockStore::default();
        assert_eq!(
            cache.get_or_load(&store, key(1, "OPEN", 25)).await.unwrap(),
            None
        );
    }

    #[tokio::test]
    async fn get_or_load_serves_from_cache_without_reloading() {
        let cache = RangeCache::new();
        let store = MockStore::preset(HashMap::from([(
            (1, "OPEN".to_string(), 25),
            stored(0.5, 30),
        )]));

        let first = cache.get_or_load(&store, key(1, "OPEN", 25)).await.unwrap();
        assert!(first.is_some());
        assert_eq!(*first.unwrap(), stored(0.5, 30));

        store.data.lock().unwrap().clear();
        let second = cache.get_or_load(&store, key(1, "OPEN", 25)).await.unwrap();
        assert_eq!(second.unwrap().as_ref(), &stored(0.5, 30));
    }

    #[tokio::test]
    async fn get_or_load_propagates_store_errors() {
        let cache = RangeCache::new();
        let store = MockStore::fail_always();
        assert!(matches!(
            cache.get_or_load(&store, key(1, "OPEN", 25)).await,
            Err(Error::Store(_))
        ));
    }

    #[tokio::test]
    async fn put_caches_immediately_and_writes_back_through_store() {
        let cache = RangeCache::new();
        let store = MockStore::default();
        cache
            .put(store.clone(), key(7, "NODE", 10), stored(0.25, 5))
            .await;

        assert_eq!(
            *cache
                .get_or_load(&store, key(7, "NODE", 10))
                .await
                .unwrap()
                .unwrap(),
            stored(0.25, 5)
        );

        wait_until(|| store.keys().contains(&(7, "NODE".to_string(), 10))).await;
        assert_eq!(
            store
                .data
                .lock()
                .unwrap()
                .get(&(7, "NODE".to_string(), 10))
                .cloned(),
            Some(stored(0.25, 5))
        );
    }

    #[tokio::test]
    async fn put_survives_store_failures() {
        let cache = RangeCache::new();
        let store = MockStore::fail_always();
        cache
            .put(store.clone(), key(7, "NODE", 25), stored(0.25, 5))
            .await;

        wait_until(|| store.attempts.load(Ordering::SeqCst) >= 1).await;
        assert_eq!(
            *cache
                .get_or_load(&store, key(7, "NODE", 25))
                .await
                .unwrap()
                .unwrap(),
            stored(0.25, 5)
        );
    }

    #[tokio::test]
    async fn cache_evicts_all_when_full_and_refreshes_existing_keys() {
        let cache = RangeCache::new();
        let store = MockStore::noop_writes();

        for i in 0..MAX_ENTRIES as i32 {
            let node = format!("NODE_{i}");
            cache
                .put(store.clone(), key(i, &node, 25), stored(i as f32, 0))
                .await;
        }

        let existing = MAX_ENTRIES as i32 - 1;
        let existing_node = format!("NODE_{existing}");
        cache
            .put(
                store.clone(),
                key(existing, &existing_node, 25),
                stored(9999.0, 0),
            )
            .await;
        assert_eq!(
            *cache
                .get_or_load(&store, key(existing, &existing_node, 25))
                .await
                .unwrap()
                .unwrap(),
            stored(9999.0, 0)
        );

        let newcomer = MAX_ENTRIES as i32;
        let newcomer_node = format!("NODE_{newcomer}");
        cache
            .put(
                store.clone(),
                key(newcomer, &newcomer_node, 25),
                stored(1.0, 0),
            )
            .await;
        assert_eq!(
            *cache
                .get_or_load(&store, key(newcomer, &newcomer_node, 25))
                .await
                .unwrap()
                .unwrap(),
            stored(1.0, 0)
        );

        let evicted = "NODE_1000".to_string();
        assert!(
            cache
                .get_or_load(&store, key(1000, &evicted, 25))
                .await
                .unwrap()
                .is_none()
        );
    }

    #[test]
    fn range_key_distinguishes_stack_buckets() {
        let a = key(1, "NODE", 10);
        let b = key(1, "NODE", 25);
        assert_ne!(a, b);
    }

    #[test]
    fn stored_range_weights_are_full_169() {
        let range: Range = [0.0; HAND_COUNT];
        assert_eq!(range.len(), HAND_COUNT);
    }
}
