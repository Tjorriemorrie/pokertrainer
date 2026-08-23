use std::collections::HashMap;
use std::sync::Arc;

use sqlx::PgPool;
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

#[derive(Default)]
pub struct RangeCache {
    entries: RwLock<HashMap<RangeKey, Arc<RangeWeights>>>,
}

impl RangeCache {
    pub fn new() -> Self {
        Self::default()
    }

    pub async fn get_or_load(
        &self,
        pool: &PgPool,
        key: RangeKey,
    ) -> Result<Option<Arc<RangeWeights>>> {
        if let Some(weights) = self.entries.read().await.get(&key).cloned() {
            return Ok(Some(weights));
        }

        let weights = db::load_contextual_range(pool, key.profile_id, &key.node).await?;
        let arc = weights.map(Arc::new);
        if let Some(cached) = arc.clone() {
            self.insert(key, cached).await;
        }
        Ok(arc)
    }

    pub async fn put(&self, pool: PgPool, key: RangeKey, weights: RangeWeights) {
        let arc = Arc::new(weights);
        self.insert(key.clone(), arc.clone()).await;

        tokio::spawn(async move {
            if let Err(e) =
                db::upsert_contextual_range(&pool, key.profile_id, &key.node, &arc).await
            {
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
