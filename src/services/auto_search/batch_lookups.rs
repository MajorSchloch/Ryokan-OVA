use std::collections::HashMap;
use std::sync::{Arc, Mutex};

use crate::services::indexers::{self, Indexer, IndexerSearchOutcome, Release, SearchQuery};
use crate::services::nyaa::{self, SearchOptions, SearchResult};

struct CachedIndexerQuery {
    indexer: Arc<dyn Indexer>,
    releases: Vec<Release>,
}

/// Successful provider responses for one release in one upgrade scan.
/// Candidate filtering, numbering proofs and selection run afresh for each episode.
#[derive(Default)]
pub(crate) struct BatchLookups {
    nyaa: Mutex<HashMap<SearchOptions, Vec<SearchResult>>>,
    indexers: Mutex<HashMap<(i64, SearchQuery), CachedIndexerQuery>>,
    pub(super) upgrade_proofs: crate::services::source_numbering::loader::Resolver,
}

impl BatchLookups {
    pub(super) async fn nyaa(&self, options: SearchOptions) -> Result<Vec<SearchResult>, String> {
        if let Some(results) = self.nyaa.lock().unwrap().get(&options) {
            return Ok(results.clone());
        }
        let results = nyaa::search(&options, 1).await?.results;
        self.nyaa.lock().unwrap().insert(options, results.clone());
        Ok(results)
    }

    pub(super) async fn indexers(
        &self,
        indexers: &[Arc<dyn Indexer>],
        query: &SearchQuery,
    ) -> Vec<IndexerSearchOutcome> {
        let mut outcomes = HashMap::new();
        let mut pending = Vec::new();
        {
            let cached = self.indexers.lock().unwrap();
            for indexer in indexers {
                let id = indexer.id();
                if let Some(hit) = cached.get(&(id, query.clone()))
                    && Arc::ptr_eq(&hit.indexer, indexer)
                {
                    outcomes.insert(
                        id,
                        IndexerSearchOutcome {
                            indexer_id: id,
                            indexer_name: indexer.name().to_string(),
                            result: Ok(hit.releases.clone()),
                        },
                    );
                } else {
                    pending.push(indexer.clone());
                }
            }
        }

        let fresh = indexers::fan_out_search(&pending, query).await;
        let mut cached = self.indexers.lock().unwrap();
        for (indexer, outcome) in pending.into_iter().zip(fresh) {
            let id = outcome.indexer_id;
            if let Ok(releases) = &outcome.result {
                cached.insert(
                    (id, query.clone()),
                    CachedIndexerQuery {
                        indexer,
                        releases: releases.clone(),
                    },
                );
            }
            outcomes.insert(id, outcome);
        }
        // Preserve the fan-out's configured order when only some providers retry.
        indexers
            .iter()
            .filter_map(|indexer| outcomes.remove(&indexer.id()))
            .collect()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::services::indexers::IndexerCaps;
    use std::sync::atomic::{AtomicUsize, Ordering};

    struct TestIndexer {
        id: i64,
        requests: AtomicUsize,
        fail_first: bool,
    }

    #[async_trait::async_trait]
    impl Indexer for TestIndexer {
        fn id(&self) -> i64 {
            self.id
        }

        fn name(&self) -> &str {
            "Test indexer"
        }

        fn priority(&self) -> i32 {
            0
        }

        fn is_private_tracker(&self) -> bool {
            false
        }

        async fn caps(&self) -> Result<IndexerCaps, String> {
            unreachable!("batch lookups only use search")
        }

        async fn search(&self, _: &SearchQuery) -> Result<Vec<Release>, String> {
            let request = self.requests.fetch_add(1, Ordering::Relaxed);
            if request == 0 && self.fail_first {
                return Err("temporary outage".into());
            }
            Ok(Vec::new())
        }
    }

    #[tokio::test]
    async fn batch_lookup_reuses_empty_success_and_retries_only_the_failed_indexer() {
        let stable = Arc::new(TestIndexer {
            id: 1,
            requests: AtomicUsize::new(0),
            fail_first: false,
        });
        let transient = Arc::new(TestIndexer {
            id: 2,
            requests: AtomicUsize::new(0),
            fail_first: true,
        });
        let indexers: Vec<Arc<dyn Indexer>> = vec![stable.clone(), transient.clone()];
        let query = SearchQuery {
            q: "A series batch".into(),
            ..Default::default()
        };
        let lookups = BatchLookups::default();
        let first = lookups.indexers(&indexers, &query).await;
        assert!(first[0].result.as_ref().is_ok_and(Vec::is_empty));
        assert!(first[1].result.is_err());

        for _ in 0..2 {
            let outcomes = lookups.indexers(&indexers, &query).await;
            assert_eq!(
                outcomes.iter().map(|r| r.indexer_id).collect::<Vec<_>>(),
                [1, 2]
            );
            assert!(
                outcomes
                    .iter()
                    .all(|r| r.result.as_ref().is_ok_and(Vec::is_empty))
            );
        }
        assert_eq!(stable.requests.load(Ordering::Relaxed), 1);
        assert_eq!(transient.requests.load(Ordering::Relaxed), 2);

        BatchLookups::default().indexers(&indexers, &query).await;
        assert_eq!(stable.requests.load(Ordering::Relaxed), 2);
        assert_eq!(transient.requests.load(Ordering::Relaxed), 3);
    }
}
