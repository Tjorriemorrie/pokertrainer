use std::collections::HashMap;
use std::future::Future;
use std::sync::Arc;

use tokio::sync::RwLock;

use crate::db::{self, RANGE_SIZE};
use crate::error::Result;

const MAX_ENTRIES: usize = 4096;

pub type RangeWeights = [f32; RANGE_SIZE];

#[derive(Clone, Debug, Hash, PartialEq, Eq)]
pub struct RangeKey {
    pub profile_id: i32,
    pub node: String,
}

pub trait RangeStore {
    fn load_range(
        &self,
        profile_id: i32,
        node: &str,
    ) -> impl Future<Output = Result<Option<RangeWeights>>> + Send;

    fn store_range(
        &self,
        profile_id: i32,
        node: &str,
        weights: &RangeWeights,
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
    async fn load_range(&self, profile_id: i32, node: &str) -> Result<Option<RangeWeights>> {
        db::load_contextual_range(&self.pool, profile_id, node).await
    }

    async fn store_range(&self, profile_id: i32, node: &str, weights: &RangeWeights) -> Result<()> {
        db::upsert_contextual_range(&self.pool, profile_id, node, weights).await
    }
}

#[derive(Default)]
pub struct RangeCache {
    entries: RwLock<HashMap<RangeKey, Arc<RangeWeights>>>,
}

impl RangeCache {
    pub fn new() -> Self {
        Self::default()
    }

    pub async fn get_or_load<S: RangeStore>(
        &self,
        store: &S,
        key: RangeKey,
    ) -> Result<Option<Arc<RangeWeights>>> {
        if let Some(weights) = self.entries.read().await.get(&key).cloned() {
            return Ok(Some(weights));
        }

        let weights = store.load_range(key.profile_id, &key.node).await?;
        let arc = weights.map(Arc::new);
        if let Some(cached) = arc.clone() {
            self.insert(key, cached).await;
        }
        Ok(arc)
    }

    pub async fn put<S>(&self, store: S, key: RangeKey, weights: RangeWeights)
    where
        S: RangeStore + Send + 'static,
    {
        let arc = Arc::new(weights);
        self.insert(key.clone(), arc.clone()).await;

        tokio::spawn(async move {
            if let Err(e) = store.store_range(key.profile_id, &key.node, &arc).await {
                tracing::warn!(
                    error = %e,
                    profile_id = key.profile_id,
                    node = %key.node,
                    "range write-back to database failed"
                );
            }
        });
    }

    async fn insert(&self, key: RangeKey, weights: Arc<RangeWeights>) {
        let mut entries = self.entries.write().await;
        if entries.len() >= MAX_ENTRIES && !entries.contains_key(&key) {
            entries.clear();
        }
        entries.insert(key, weights);
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

    #[derive(Clone)]
    struct MockStore {
        data: Arc<Mutex<HashMap<(i32, String), RangeWeights>>>,
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
        fn preset(entries: HashMap<(i32, String), RangeWeights>) -> Self {
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

        fn keys(&self) -> Vec<(i32, String)> {
            self.data.lock().unwrap().keys().cloned().collect()
        }
    }

    impl RangeStore for MockStore {
        async fn load_range(&self, profile_id: i32, node: &str) -> Result<Option<RangeWeights>> {
            if self.failures.load(Ordering::SeqCst) {
                return Err(Error::Store("mock load failure".to_string()));
            }
            Ok(self
                .data
                .lock()
                .unwrap()
                .get(&(profile_id, node.to_string()))
                .copied())
        }

        async fn store_range(
            &self,
            profile_id: i32,
            node: &str,
            weights: &RangeWeights,
        ) -> Result<()> {
            self.attempts.fetch_add(1, Ordering::SeqCst);
            if self.failures.load(Ordering::SeqCst) {
                return Err(Error::Store("mock store failure".to_string()));
            }
            if self.writes_enabled.load(Ordering::SeqCst) {
                self.data
                    .lock()
                    .unwrap()
                    .insert((profile_id, node.to_string()), *weights);
            }
            Ok(())
        }
    }

    fn key(id: i32, node: &str) -> RangeKey {
        RangeKey {
            profile_id: id,
            node: node.to_string(),
        }
    }

    fn weights(fill: f32) -> RangeWeights {
        [fill; RANGE_SIZE]
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
            cache.get_or_load(&store, key(1, "OPEN")).await.unwrap(),
            None
        );
    }

    #[tokio::test]
    async fn get_or_load_serves_from_cache_without_reloading() {
        let cache = RangeCache::new();
        let store = MockStore::preset(HashMap::from([((1, "OPEN".to_string()), weights(0.5))]));

        let first = cache.get_or_load(&store, key(1, "OPEN")).await.unwrap();
        assert!(first.is_some());
        assert_eq!(*first.unwrap(), weights(0.5));

        store.data.lock().unwrap().clear();
        let second = cache.get_or_load(&store, key(1, "OPEN")).await.unwrap();
        assert_eq!(second.unwrap().as_ref(), &weights(0.5));
    }

    #[tokio::test]
    async fn get_or_load_propagates_store_errors() {
        let cache = RangeCache::new();
        let store = MockStore::fail_always();
        assert!(matches!(
            cache.get_or_load(&store, key(1, "OPEN")).await,
            Err(Error::Store(_))
        ));
    }

    #[tokio::test]
    async fn put_caches_immediately_and_writes_back_through_store() {
        let cache = RangeCache::new();
        let store = MockStore::default();
        cache
            .put(store.clone(), key(7, "NODE"), weights(0.25))
            .await;

        assert_eq!(
            *cache
                .get_or_load(&store, key(7, "NODE"))
                .await
                .unwrap()
                .unwrap(),
            weights(0.25)
        );

        wait_until(|| store.keys().contains(&(7, "NODE".to_string()))).await;
        assert_eq!(
            store
                .data
                .lock()
                .unwrap()
                .get(&(7, "NODE".to_string()))
                .copied(),
            Some(weights(0.25))
        );
    }

    #[tokio::test]
    async fn put_survives_store_failures() {
        let cache = RangeCache::new();
        let store = MockStore::fail_always();
        cache
            .put(store.clone(), key(7, "NODE"), weights(0.25))
            .await;

        wait_until(|| store.attempts.load(Ordering::SeqCst) >= 1).await;
        assert_eq!(
            *cache
                .get_or_load(&store, key(7, "NODE"))
                .await
                .unwrap()
                .unwrap(),
            weights(0.25)
        );
    }

    #[tokio::test]
    async fn cache_evicts_all_when_full_and_refreshes_existing_keys() {
        let cache = RangeCache::new();
        let store = MockStore::noop_writes();

        for i in 0..MAX_ENTRIES as i32 {
            let node = format!("NODE_{i}");
            cache
                .put(store.clone(), key(i, &node), weights(i as f32))
                .await;
        }

        let existing = MAX_ENTRIES as i32 - 1;
        let existing_node = format!("NODE_{existing}");
        cache
            .put(
                store.clone(),
                key(existing, &existing_node),
                weights(9999.0),
            )
            .await;
        assert_eq!(
            *cache
                .get_or_load(&store, key(existing, &existing_node))
                .await
                .unwrap()
                .unwrap(),
            weights(9999.0)
        );

        let newcomer = MAX_ENTRIES as i32;
        let newcomer_node = format!("NODE_{newcomer}");
        cache
            .put(store.clone(), key(newcomer, &newcomer_node), weights(1.0))
            .await;
        assert_eq!(
            *cache
                .get_or_load(&store, key(newcomer, &newcomer_node))
                .await
                .unwrap()
                .unwrap(),
            weights(1.0)
        );

        let evicted = "NODE_1000".to_string();
        assert!(
            cache
                .get_or_load(&store, key(1000, &evicted))
                .await
                .unwrap()
                .is_none()
        );
    }
}
